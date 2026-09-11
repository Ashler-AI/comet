import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtemp, mkdir, writeFile, rm, readFile } from 'node:fs/promises';
import { createContext, runInContext } from 'node:vm';
import { randomUUID } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer, Viewport, loadConfig } from './server.mjs';
import { applyTranscriptFrame } from './rpc.mjs';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'crew-web-test-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = { sessionId: 'session-a', sandboxId: 'sandbox-a', token: 'x'.repeat(40), storage: join(root, 'web'), runtimeDir: root };
  await mkdir(join(root, 'omp-inference'));
  await writeFile(join(root, 'omp-inference', 'profile.json'), JSON.stringify({ profile: 'scaffold-host', model: 'scaffold-openai/gpt-example' }));
  const rpc = new EventEmitter();
  rpc.close = () => {};
  const commands = new Map();
  rpc.call = async (method, params) => {
    if (method === 'ReadSessionAuthority') return {
      scope: { projectId: 'project-a', deploymentId: 'deployment-a', sessionId: 'session-a' }, sandboxId: 'sandbox-a',
      deviceId: 'device-a', principalSubject: 'owner-a', grantId: 'grant-a', expiresAt: Date.now() + 60000,
      capabilities: ['session.read', 'session.chat', 'session.control'],
    };
    if (method === 'QueueCommand') {
      if (!commands.has(params.commandId)) commands.set(params.commandId, params);
      return { commandId: params.commandId };
    }
    if (method === 'ReadSessionCommand') {
      const entry = commands.get(params.commandId);
      return { command: entry ? { commandId: params.commandId, status: entry.status || 'pending', resolution: entry.resolution || null } : null };
    }
    throw new Error(`Unexpected test RPC ${method}`);
  };
  const viewport = new Viewport(config, rpc);
  viewport.connection = 'connected';
  viewport.snapshots = new Set(['chat', 'sessions', 'selection', 'messages', 'collaboration']);
  viewport.chat = { id: 'session-a', deviceId: 'device-a', cwd: '/workspace', config: { model: 'openai-codex/gpt-example', reasoning: 'high', sandbox: 'workspace-write' } };
  const { server } = await createServer(config, viewport);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); return new Promise(resolve => server.close(resolve)); });
  const url = `http://127.0.0.1:${server.address().port}`;
  const headers = { authorization: `Basic ${Buffer.from(`opencode:${config.token}`).toString('base64')}`, 'content-type': 'application/json', 'x-crew-web-origin-verified': 'true' };
  const post = (path, value, customHeaders = headers) => fetch(url + path, { method: 'POST', headers: customHeaders, body: JSON.stringify(value) });
  return { config, rpc, viewport, commands, url, headers, post };
}

const message = { requestId: 'request-a', text: 'Continue', model: 'openai-codex/gpt-example', reasoning: 'high', attachments: [] };

test('HTTP boundary denies missing credentials, unverified writes, raw RPC and caller-selected sessions', async t => {
  const f = await fixture(t);
  assert.equal((await fetch(f.url + '/api/session')).status, 401);
  assert.equal((await f.post('/api/message', message, { ...f.headers, 'x-crew-web-origin-verified': '' })).status, 403);
  assert.equal((await f.post('/api/rpc', { method: 'Mutate' })).status, 404);
  assert.equal((await f.post('/api/message', { ...message, chatId: 'foreign' })).status, 400);
  assert.equal(f.commands.size, 0);
  const health = await fetch(f.url + '/health', { headers: f.headers });
  assert.deepEqual(await health.json(), { service: 'crew-web', sessionId: 'session-a', sandboxId: 'sandbox-a' });
  assert.equal((await fetch(f.url + '/healthz', { headers: f.headers })).status, 404);
});

test('follow-ups leave native continuation to the engine for the assigned session', async t => {
  const f = await fixture(t);
  f.viewport.chat.harnessSessionId = 'stale-chat-native';
  f.viewport.collaboration = { sessions: [{ sessionId: 'session-a', harnessSessionId: 'stale-projected-native' }] };
  assert.equal((await f.post('/api/message', message)).status, 202);
  const entry = [...f.commands.values()][0];
  assert.equal(entry.chatId, 'session-a');
  assert.equal(entry.command.sessionId, 'session-a');
  assert.equal(entry.command.action.request.resume, null);
});

test('browser restores the authorized route after failed admission or rejection and preserves retries', async () => {
  const source = await readFile(new URL('./public/app.js', import.meta.url), 'utf8');
  for (const failure of ['admission', 'rejection', 'outcome']) {
    const nodes = new Map();
    const node = id => {
      if (!nodes.has(id)) nodes.set(id, {
        value: '', style: {}, dataset: {}, options: [], listeners: {},
        addEventListener(name, listener) { this.listeners[name] = listener; },
        querySelectorAll() { return []; }, replaceChildren() {}, focus() {},
      });
      return nodes.get(id);
    };
    const calls = [];
    let fail = true;
    let route = 'gpt-example';
    const context = createContext({
      document: { getElementById: node, querySelector: node },
      window: { addEventListener() {} },
      ResizeObserver: class { observe() {} },
      EventSource: class { addEventListener() {} },
      crypto: { randomUUID },
      fetch: async (path, options) => {
        if (path === './api/session' || path === './api/models') return new Promise(() => {});
        const body = options.body && JSON.parse(options.body);
        calls.push({ path, body });
        let status = 200;
        let result = {};
        if (path.endsWith('/model-route')) route = body.model;
        else if (path === './api/message') {
          if (fail && failure === 'admission') status = 503;
          else if (route !== body.model.split('/')[1]) status = 409;
        } else {
          if (fail && failure === 'outcome') status = 503;
          result = { status: fail && failure === 'rejection' ? 'rejected' : 'applied' };
        }
        return new Response(JSON.stringify(result), { status, headers: { 'content-type': 'application/json' } });
      },
    });
    runInContext(source, context);
    runInContext(`state = { connection: 'connected', sandboxId: 'sandbox-a', session: { model: 'openai-codex/gpt-example', reasoning: 'high', status: 'idle' }, capabilities: { message: true }, models: [{}] }; connected = true;`, context);
    node('message').value = 'Continue';
    node('model').value = 'openai-codex/gpt-other';
    node('reasoning').value = 'high';
    const submit = () => node('composer').listeners.submit({ preventDefault() {} });
    await submit();
    assert.equal(node('message').value, 'Continue');
    fail = false;
    if (failure === 'outcome') {
      await submit();
      const messages = calls.filter(call => call.path === './api/message');
      assert.equal(messages[0].body.requestId, messages[1].body.requestId);
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 1);
    } else {
      node('model').value = 'openai-codex/gpt-example';
      await submit();
      assert.equal(route, 'gpt-example');
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 2);
    }
    assert.equal(node('message').value, '');
  }
});

test('durable request retry does not turn a started message into a steer or duplicate after restart', async t => {
  const f = await fixture(t);
  const first = await f.post('/api/message', message);
  assert.equal(first.status, 202);
  const receipt = await first.json();
  f.viewport.live = { status: 'working' };
  const retry = await f.post('/api/message', message);
  assert.deepEqual(await retry.json(), receipt);
  assert.equal(f.commands.size, 1);
  assert.equal([...f.commands.values()][0].command.action.action, 'start');
  const restarted = new Viewport(f.config, f.rpc);
  assert.deepEqual(await restarted.enqueue('message', message), receipt);
  assert.equal((await f.post('/api/message', { ...message, text: 'Different' })).status, 409);
  assert.equal(f.commands.size, 1);
});

test('model route mismatch, foreign attachments and stale approval cannot admit commands', async t => {
  const f = await fixture(t);
  assert.equal((await f.post('/api/message', { ...message, model: 'anthropic/other-model' })).status, 409);
  assert.equal((await f.post('/api/message', { ...message, attachments: [{ id: 'unknown' }] })).status, 400);
  assert.equal((await f.post('/api/input', { requestId: 'approval-a', inputRequestId: 'stale', answers: [] })).status, 409);
  assert.equal(f.commands.size, 0);
});

test('incomplete reconnection snapshots cannot select a stale run mode', async t => {
  const f = await fixture(t);
  f.viewport.live = { status: 'working' };
  f.viewport.snapshots.delete('selection');
  assert.equal((await f.post('/api/message', message)).status, 503);
  assert.equal(f.commands.size, 0);
});

test('renewed authority restores capabilities while changed host identity fails closed', async t => {
  const f = await fixture(t);
  await f.viewport.authority('session.read');
  f.viewport.currentAuthority.expiresAt = 0;
  assert.equal(f.viewport.state().capabilities.message, false);
  await f.viewport.authority('session.read');
  assert.equal(f.viewport.state().capabilities.message, true);
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => method === 'ReadSessionAuthority'
    ? { ...(await original(method, params)), deviceId: 'foreign-device' } : original(method, params);
  await assert.rejects(f.viewport.authority(), /binding changed/);
});

test('foreign or expired engine authority fails before command admission', async t => {
  const f = await fixture(t);
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => method === 'ReadSessionAuthority' ? {
    ...(await original(method, params)), scope: { sessionId: 'foreign' },
  } : original(method, params);
  assert.equal((await f.post('/api/message', message)).status, 503);
  assert.equal(f.commands.size, 0);
  f.rpc.call = async (method, params) => method === 'ReadSessionAuthority' ? {
    ...(await original(method, params)), expiresAt: 0,
  } : original(method, params);
  assert.equal((await f.post('/api/message', message)).status, 503);
  assert.equal(f.commands.size, 0);
});

test('lost read capability denies cached session, streams, history and tool reads', async t => {
  const f = await fixture(t);
  await f.viewport.readAuthority();
  f.viewport.messages = [{ id: 'private-message', parts: [{ kind: 'text', id: 'p', text: 'protected text' }] }];
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => method === 'ReadSessionAuthority'
    ? { ...(await original(method, params)), capabilities: ['session.chat'] } : original(method, params);
  for (const path of ['/api/session', '/api/events', '/api/messages?before=10', '/api/tool/private-tool']) {
    const response = await fetch(f.url + path, { headers: f.headers });
    assert.equal(response.status, 403);
    assert.equal((await response.text()).includes('protected text'), false);
  }
  assert.deepEqual(f.viewport.messages, []);
  assert.equal(f.viewport.state().transcriptReset, true);
});

test('an existing event subscriber receives only a redacted reset after read revocation', async t => {
  const f = await fixture(t);
  await f.viewport.readAuthority();
  f.viewport.messages = [{ id: 'secret', parts: [{ kind: 'text', text: 'protected transcript' }] }];
  let ended;
  f.viewport.clients.add({ writableLength: 0, write() { assert.fail('Protected event must not be written'); }, end(frame) { ended = frame; } });
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => method === 'ReadSessionAuthority'
    ? { ...(await original(method, params)), capabilities: [] } : original(method, params);
  await f.viewport.emit();
  assert.equal(ended.includes('protected transcript'), false);
  const state = JSON.parse(ended.split('data: ')[1].trim());
  assert.deepEqual(state.messages, []);
  assert.equal(state.transcriptReset, true);
  assert.equal(state.connection, 'disconnected');
});

test('admission remains pending until the durable command outcome exposes rejection', async t => {
  const f = await fixture(t);
  assert.equal((await f.post('/api/message', message)).status, 202);
  const outcomeUrl = f.url + '/api/command/' + message.requestId;
  assert.equal((await (await fetch(outcomeUrl, { headers: f.headers })).json()).status, 'pending');
  const entry = [...f.commands.values()][0];
  entry.status = 'rejected';
  entry.resolution = 'Control grant was revoked before execution';
  const rejected = await (await fetch(outcomeUrl, { headers: f.headers })).json();
  assert.equal(rejected.status, 'rejected');
  assert.equal(rejected.resolution, entry.resolution);
  assert.equal((await fetch(f.url + '/api/command/foreign-command', { headers: f.headers })).status, 404);
  assert.equal(f.commands.size, 1);
});

test('bounded HTTP body rejects oversize payload without admission', async t => {
  const f = await fixture(t);
  const response = await f.post('/api/message', { ...message, text: 'a'.repeat(256 * 1024) });
  assert.equal(response.status, 413);
  assert.equal(f.commands.size, 0);
});

test('trusted configuration rejects conflicting session bindings and non-loopback IPC targets', () => {
  const env = { CREW_WEB_SESSION_ID: 'a', SCAFFOLD_COMET_RUNTIME_PROFILE_JSON: '{"sessionId":"b"}', CREW_WEB_AUTH_TOKEN: 'a'.repeat(40), COMET_IPC_PORT: '39400', COMET_DATA_DIR: '/data', SCAFFOLD_RUNTIME_DIR: '/runtime' };
  assert.throws(() => loadConfig(env), /Conflicting session/);
  assert.throws(() => loadConfig({ ...env, CREW_WEB_SESSION_ID: 'b', COMET_IPC_PORT: 'remote:39400' }), /COMET_IPC_PORT/);
});

test('transcript deltas preserve UTF-8 byte windows and reject desynchronization', () => {
  const initial = [{ id: 'a', parts: [{ kind: 'text', id: 'p', text: 'éhi' }] }];
  const updated = applyTranscriptFrame(initial, { append: [{ entry: 'a', part: 'p', drop_prefix: 2, text: '!', omitted_prefix_bytes: 2, len: 3 }], count: 1 });
  assert.deepEqual(updated[0].parts[0], { kind: 'textWindow', id: 'p', text: 'hi!', omitted_prefix_bytes: 2 });
  assert.throws(() => applyTranscriptFrame(updated, { count: 3 }), /count mismatch/);
  assert.throws(() => applyTranscriptFrame(updated, { upsert: [{ after: 'missing', entry: { id: 'b', parts: [] } }], count: 2 }), /anchor missing/);
});
