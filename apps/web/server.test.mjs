import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer, Viewport, loadConfig, browserEntry } from './server.mjs';
import { CrewRpc, applyTranscriptFrame } from './rpc.mjs';

const profile = {
  projectId: 'crew-upstream-canary', deploymentId: 'crew-upstream-canary', sessionId: 'session-a',
  sandboxId: 'sandbox-a', targetDeviceId: 'device-a', lifecycleEpoch: 1, actor: { sub: 'owner@example.com' },
};
const message = { requestId: 'request-a', text: 'Continue', model: 'openai-codex/gpt-example', reasoning: 'high', attachments: [] };
const entry = (id, text = 'hello') => ({ id, role: 'assistant', createdAt: 1000, status: 'complete', parts: [{ id: `${id}:text`, kind: 'text', text }] });

async function fixture(t, publish = true) {
  const root = await mkdtemp(join(tmpdir(), 'crew-native-web-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const env = {
    SCAFFOLD_CREW_SYNC_GENERATION: 'chat2', SCAFFOLD_COMET_RUNTIME_PROFILE_JSON: JSON.stringify(profile),
    CREW_WEB_AUTH_TOKEN: 'private-token-'.repeat(4), COMET_IPC_PORT: '27954',
    COMET_DATA_DIR: join(root, '.scaffold/crew-upstream-canary/data'), SCAFFOLD_RUNTIME_DIR: root,
  };
  const config = loadConfig(env);
  await mkdir(join(root, 'omp-inference'));
  await writeFile(join(root, 'omp-inference/profile.json'), JSON.stringify({ profile: 'scaffold-host', model: 'scaffold-openai/gpt-example' }));
  const authority = {
    scope: { projectId: profile.projectId, deploymentId: profile.deploymentId, sessionId: profile.sessionId },
    sandboxId: profile.sandboxId, deviceId: profile.targetDeviceId, lifecycleEpoch: 1,
    principalSubject: profile.actor.sub, grantId: 'private-grant', expiresAt: Date.now() + 60000,
    capabilities: ['session.read', 'session.chat', 'session.control', 'session.files'],
  };
  const context = {
    id: profile.sessionId, deviceId: profile.targetDeviceId, roomGen: 2, cwd: '/workspace', title: 'Assigned chat',
    config: { harness: 'omp', model: message.model, reasoning: message.reasoning, sandbox: 'workspace-write', modelOptions: { trusted: true } },
    crew: { ownerSubject: 'opaque-edge-owner', sessionId: profile.sessionId, scope: authority.scope },
  };
  const commands = new Map();
  const calls = [];
  const watches = new Map();
  const rpc = new EventEmitter();
  rpc.connect = async () => {};
  rpc.close = () => {};
  rpc.watch = (method, params, callback) => { watches.set(method, { params, callback }); return () => watches.delete(method); };
  rpc.call = async (method, params) => {
    calls.push({ method, params });
    if (method === 'ScaffoldHostAuthority') return structuredClone(authority);
    if (method === 'ListModels') return [{ id: message.model, label: 'Example', reasoningLevels: ['high'] }];
    if (method === 'QueueCommand') {
      if (commands.has(params.commandId)) assert.deepEqual(commands.get(params.commandId).payload, params.command);
      else commands.set(params.commandId, { id: params.commandId, status: 'pending', resolution: null, payload: params.command });
      return { commandId: params.commandId };
    }
    if (method === 'ReadCommand') return { command: commands.get(params.commandId) ?? null };
    if (method === 'UploadChunk') { assert.ok(Buffer.from(params.data, 'base64').length <= 45000); return {}; }
    if (method === 'UploadCommit') return { path: `pending://${params.uploadId}/${params.fileName}` };
    if (method === 'FetchToolBlob') return { text: 'captured output' };
    assert.fail(`Unexpected native RPC ${method}`);
  };
  const viewport = new Viewport(config, rpc);
  const { server } = await createServer(config, viewport);
  await viewport.connect();
  const publishSnapshots = () => {
    watches.get('WatchChats').callback([context]);
    watches.get('WatchSessions').callback([]);
    watches.get('WatchDocMessages').callback({ reset: [] });
  };
  if (publish) publishSnapshots();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { viewport.stop(); server.closeAllConnections(); return new Promise(resolve => server.close(resolve)); });
  const url = `http://127.0.0.1:${server.address().port}`;
  const headers = { authorization: `Basic ${Buffer.from(`opencode:${config.token}`).toString('base64')}`, 'content-type': 'application/json', 'x-crew-web-origin-verified': 'true' };
  const get = path => fetch(url + path, { headers });
  const post = (path, input, customHeaders = headers) => fetch(url + path, { method: 'POST', headers: customHeaders, body: JSON.stringify(input) });
  return { env, config, authority, context, commands, calls, watches, rpc, viewport, publishSnapshots, url, headers, get, post };
}

test('only trusted chat2 configuration enables the isolated bridge', async t => {
  const f = await fixture(t);
  assert.equal(f.config.ipcPort, 27954);
  for (const change of [
    { SCAFFOLD_CREW_SYNC_GENERATION: '' }, { SCAFFOLD_CREW_SYNC_GENERATION: 'legacy' },
    { COMET_IPC_PORT: '39400' }, { COMET_DATA_DIR: '/tmp/shared-data' },
    { CREW_WEB_AUTH_TOKEN: 'short' }, { CREW_WEB_SESSION_ID: 'another-session' },
    { SCAFFOLD_COMET_RUNTIME_PROFILE_JSON: JSON.stringify({ ...profile, actor: {} }) },
  ]) assert.throws(() => loadConfig({ ...f.env, ...change }));
  assert.throws(() => loadConfig({ ...f.env, SCAFFOLD_CREW_SYNC_GENERATION: '', SCAFFOLD_COMET_RUNTIME_PROFILE_JSON: JSON.stringify({ ...profile, crewSyncGeneration: 'chat2' }) }));
});

test('real health requires authority and all native snapshots; private native opening contains no credentials', async t => {
  const f = await fixture(t, false);
  assert.equal((await f.get('/health')).status, 503);
  f.publishSnapshots();
  assert.equal((await f.get('/health')).status, 200);
  assert.deepEqual([...f.watches.keys()], ['WatchChats', 'WatchSessions', 'WatchDocMessages']);
  assert.deepEqual(f.watches.get('WatchDocMessages').params, { chatId: profile.sessionId });
  const state = await (await f.get('/api/session')).json();
  assert.equal(state.capabilities.message, true);
  assert.equal(state.session.cwd, '/workspace');
  assert.equal(JSON.stringify(state).includes('private-grant'), false);
  assert.equal(JSON.stringify(state).includes('opaque-edge-owner'), false);
  assert.deepEqual(await (await f.get('/api/native-open')).json(), {
    url: 'crew-upstream-canary://scaffold/crew-upstream-canary/crew-upstream-canary/session-a/sandbox-a',
  });
  assert.throws(() => f.watches.get('WatchChats').callback([{ ...f.context, deviceId: 'other-device' }]));
  assert.equal(f.viewport.state().capabilities.message, false);
});

test('proxy authentication, mutation origin, bounded bodies and exact grant binding fail closed', async t => {
  const f = await fixture(t);
  assert.equal((await fetch(f.url + '/api/session')).status, 401);
  assert.equal((await f.post('/api/message', message, { ...f.headers, 'x-crew-web-origin-verified': 'false' })).status, 403);
  assert.equal((await f.post('/api/message', { ...message, cwd: '/outside' })).status, 400);
  assert.equal((await f.post('/api/message', { ...message, text: 'x'.repeat(300000) })).status, 413);
  assert.equal(f.commands.size, 0);
  for (const change of [
    { principalSubject: 'other@example.com' }, { deviceId: 'other-device' }, { lifecycleEpoch: 2 },
    { sandboxId: 'other-sandbox' }, { expiresAt: 0 }, { grantId: '' },
    { scope: { ...f.authority.scope, projectId: 'other-project' } },
    { scope: { ...f.authority.scope, deploymentId: 'other-deployment' } },
    { scope: { ...f.authority.scope, sessionId: 'other-session' } },
  ]) assert.throws(() => f.viewport.acceptAuthority({ ...f.authority, ...change }, 'session.read'));
  f.authority.capabilities = ['session.read'];
  assert.equal((await f.post('/api/message', message)).status, 403);
});

test('durable native command retries survive lost receipt and bridge restart without duplicate execution', async t => {
  const f = await fixture(t);
  const call = f.rpc.call;
  let loseReceipt = true;
  f.rpc.call = async (method, params) => {
    const result = await call(method, params);
    if (method === 'QueueCommand' && loseReceipt) { loseReceipt = false; throw new Error('Connection lost after durable admission'); }
    return result;
  };
  assert.equal((await f.post('/api/message', message)).status, 503);
  const restarted = new Viewport(f.config, f.rpc);
  restarted.context = f.context;
  restarted.connection = 'connected';
  restarted.snapshots = new Set(['context', 'sessions', 'messages']);
  const receipt = await restarted.enqueue('message', message);
  assert.deepEqual(receipt, { commandId: 'crew-web:1:request-a' });
  assert.equal(f.commands.size, 1);
  const stored = f.commands.get(receipt.commandId);
  assert.equal(stored.payload.kind, 'run');
  assert.equal(stored.payload.messageId, receipt.commandId);
  assert.equal(stored.payload.request.cwd, '/workspace');
  assert.equal(stored.payload.request.harness, 'omp');
  assert.deepEqual(stored.payload.request.modelOptions, { trusted: true });
  assert.equal(stored.payload.request.resume, null);
  assert.equal((await f.post('/api/message', { ...message, text: 'different' })).status, 409);
  stored.status = 'applied';
  const outcome = await (await f.get('/api/command/request-a')).json();
  assert.deepEqual(outcome, { commandId: receipt.commandId, status: 'applied', resolution: null });
  f.authority.capabilities = ['session.read'];
  assert.equal((await f.post('/api/message', message)).status, 403);
});

test('steer, input and interruption use native payloads and bind cancellation to the current transcript turn', async t => {
  const f = await fixture(t);
  f.watches.get('WatchSessions').callback([{ chatId: profile.sessionId, deviceId: profile.targetDeviceId, status: 'working', startedAt: '2026-01-01T00:00:00Z' }]);
  f.watches.get('WatchDocMessages').callback({ reset: [entry('turn-a')] });
  assert.equal((await f.post('/api/message', message)).status, 202);
  assert.equal(f.commands.get('crew-web:1:request-a').payload.kind, 'steer');
  assert.equal((await f.post('/api/interrupt', { requestId: 'stop-old', turnId: 'old-turn' })).status, 409);
  assert.equal((await f.post('/api/interrupt', { requestId: 'stop-a', turnId: 'turn-a' })).status, 202);
  const stop = f.calls.find(row => row.method === 'QueueCommand' && row.params.command.kind === 'interrupt');
  assert.equal(stop.params.expectedTurnId, 'turn-a');
  assert.deepEqual(stop.params.command, { kind: 'interrupt' });
  f.viewport.messages = [{ ...entry('question'), parts: [{ id: 'input-a', kind: 'input', requestId: 'input-a', resolved: false, questions: [{ id: 'q', multiSelect: false }] }] }];
  assert.equal((await f.post('/api/input', { requestId: 'answer-a', inputRequestId: 'input-a', answers: [{ questionId: 'q', labels: ['yes'] }] })).status, 202);
  assert.equal(f.commands.get('crew-web:1:answer-a').payload.kind, 'respondInput');
  assert.equal(f.commands.get('crew-web:1:answer-a').payload.requestId, 'input-a');
  assert.equal((await f.post('/api/input', { requestId: 'answer-b', inputRequestId: 'missing', answers: [] })).status, 409);
});

test('uploads cannot choose filesystem paths and tool detail remains session scoped', async t => {
  const f = await fixture(t);
  const upload = name => fetch(f.url + '/api/upload', { method: 'POST', headers: { ...f.headers, 'x-filename': encodeURIComponent(name), 'content-type': 'image/png' }, body: Buffer.alloc(90001, 1) });
  assert.equal((await upload('../outside.png')).status, 400);
  const response = await upload('image.png');
  assert.equal(response.status, 201);
  const metadata = await response.json();
  assert.equal(Object.hasOwn(metadata, 'path'), false);
  assert.equal(f.calls.filter(row => row.method === 'UploadChunk').length, 3);
  assert.equal((await f.post('/api/message', { ...message, attachments: [metadata] })).status, 202);
  assert.match(f.commands.get('crew-web:1:request-a').payload.request.attachments[0], /^pending:\/\//);
  assert.equal((await f.post('/api/message', { ...message, requestId: 'bad-upload', attachments: [{ id: '/etc/passwd' }] })).status, 400);
  const part = { id: 'tool-a', kind: 'tool', call: { kind: 'exec', command: 'pwd' }, outputRef: 'session-a/tool-a', resolved: true };
  f.viewport.messages = [{ ...entry('tools'), parts: [part] }];
  assert.equal((await (await f.get('/api/tool/tool-a')).json()).output, 'captured output');
  assert.deepEqual(f.calls.find(row => row.method === 'FetchToolBlob').params, { blobRef: 'session-a/tool-a' });
  part.outputRef = 'other-session/tool-a';
  assert.equal((await f.get('/api/tool/tool-a')).status, 503);
  assert.equal(f.calls.filter(row => row.method === 'FetchToolBlob').length, 1);
  assert.equal((await f.get('/api/tool/missing')).status, 404);
});

test('native reset and streaming deltas preserve UTF-8 text and reasoning, bounded history stays pageable', async t => {
  const f = await fixture(t);
  const initial = { ...entry('a', 'é'), parts: [{ id: 'text', kind: 'text', text: 'é' }, { id: 'reason', kind: 'reasoning', text: 'private' }] };
  let rows = applyTranscriptFrame([], { reset: [initial] });
  rows = applyTranscriptFrame(rows, { append: [{ entry: 'a', part: 'text', text: '世', len: 5 }, { entry: 'a', part: 'reason', text: ' thought', len: 15 }], count: 1 });
  assert.equal(rows[0].parts[0].text, 'é世');
  assert.equal(rows[0].parts[1].kind, 'reasoning');
  assert.equal(JSON.stringify(browserEntry(rows[0])).includes('private thought'), false);
  assert.throws(() => applyTranscriptFrame(rows, { append: [{ entry: 'a', part: 'text', text: 'x', len: 999 }], count: 1 }), /length mismatch/);
  assert.throws(() => applyTranscriptFrame(rows, { upsert: [{ after: 'missing', entry: entry('b') }], count: 2 }), /anchor missing/);
  f.viewport.messages = Array.from({ length: 240 }, (_, index) => entry(String(index), 'x'.repeat(8000)));
  let before;
  const seen = [];
  do {
    const page = f.viewport.messagePage(before);
    assert.ok(Buffer.byteLength(JSON.stringify(page)) < 530000);
    seen.unshift(...page.entries.map(row => row.id));
    before = page.before;
  } while (before !== null);
  assert.deepEqual(seen, f.viewport.messages.map(row => row.id));
  const large = browserEntry(entry('huge', '世'.repeat(50000)));
  assert.equal(large.parts[0].kind, 'textWindow');
  assert.ok(Buffer.byteLength(large.parts[0].text) <= 32768);
  assert.equal(large.parts[0].text.includes('\ufffd'), false);
  assert.equal(Buffer.byteLength(large.parts[0].text) + large.parts[0].omitted_prefix_bytes, 150000);
});

test('SSE coalesces backpressure, disconnects oversized backlog, and erases state on revocation', async t => {
  const f = await fixture(t);
  const client = new EventEmitter();
  const frames = [];
  client.writableLength = 0;
  client.write = frame => { frames.push(frame); return false; };
  client.end = frame => frames.push(frame);
  client.destroy = () => assert.fail('Normal backpressure must not disconnect');
  f.viewport.clients.add(client);
  f.viewport.messages = [entry('a', 'first')];
  await f.viewport.emit();
  f.viewport.messages = [entry('a', 'latest')];
  await f.viewport.emit();
  assert.equal(frames.length, 1);
  client.emit('drain');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(JSON.parse(frames[1].split('data: ')[1]).messages[0].parts[0].text, 'latest');
  f.authority.capabilities = [];
  await f.viewport.emit();
  const redacted = JSON.parse(frames.at(-1).split('data: ')[1]);
  assert.deepEqual(redacted.messages, []);
  assert.equal(redacted.transcriptReset, true);
  assert.equal(f.viewport.clients.size, 0);
  let destroyed = false;
  client.destroy = () => { destroyed = true; };
  client.writableLength = 2 * 1024 * 1024;
  f.viewport.clients.add(client);
  f.viewport.sendState(client, 'overflow');
  assert.equal(destroyed, true);
});

test('native subscriptions cancel their own wire request without closing other streams', () => {
  const rpc = new CrewRpc(27954);
  const frames = [];
  rpc.socket = { readyState: WebSocket.OPEN, send: frame => frames.push(JSON.parse(frame)), close: () => assert.fail('Cancellation must not close the engine connection') };
  const cancel = rpc.watch('WatchDocMessages', { chatId: 'session-a' }, () => {});
  const cancelOther = rpc.watch('WatchSessions', {}, () => {});
  cancel();
  cancel();
  assert.equal(rpc.pending.size, 1);
  assert.deepEqual(frames[2], { id: 1, cancel: true });
  cancelOther();
  assert.equal(rpc.pending.size, 0);
});
