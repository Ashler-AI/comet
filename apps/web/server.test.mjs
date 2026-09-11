import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtemp, mkdir, writeFile, rm, readFile } from 'node:fs/promises';
import { createContext, runInContext } from 'node:vm';
import { randomUUID } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer, Viewport, loadConfig } from './server.mjs';
import { CrewRpc, applyTranscriptFrame } from './rpc.mjs';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'crew-web-test-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = { sessionId: 'session-a', sandboxId: 'sandbox-a', token: 'x'.repeat(40), storage: join(root, 'web'), runtimeDir: root };
  await mkdir(join(root, 'omp-inference'));
  await writeFile(join(root, 'omp-inference', 'profile.json'), JSON.stringify({ profile: 'scaffold-host', model: 'scaffold-openai/gpt-example' }));
  const rpc = new EventEmitter();
  rpc.close = () => {};
  const commands = new Map();
  const uploads = new Map();
  const context = { cwd: '/workspace', config: { harness: 'omp', model: 'openai-codex/gpt-example', reasoning: 'high', sandbox: 'workspace-write', modelOptions: { trusted: true } } };
  rpc.call = async (method, params) => {
    if (method === 'ReadSessionAuthority') return {
      scope: { projectId: 'project-a', deploymentId: 'deployment-a', sessionId: 'session-a' }, sandboxId: 'sandbox-a',
      deviceId: 'device-a', lifecycleEpoch: 1, principalSubject: 'owner-a', grantId: 'grant-a', expiresAt: Date.now() + 60000,
      capabilities: ['session.read', 'session.chat', 'session.control'],
    };
    if (method === 'ReadSessionContext') {
      assert.deepEqual(params, { chatId: config.sessionId });
      return { ...context, authority: await rpc.call('ReadSessionAuthority', params) };
    }
    if (method === 'UploadChunk') {
      const bytes = Buffer.from(params.data, 'base64');
      if (bytes.length > 45000) throw new Error('Upload chunk exceeds 45000 raw bytes');
      const chunks = uploads.get(params.uploadId) || [];
      assert.equal(params.seq, chunks.length);
      chunks.push(bytes);
      uploads.set(params.uploadId, chunks);
      return {};
    }
    if (method === 'UploadCommit') {
      assert.ok(uploads.has(params.uploadId));
      return { path: `/workspace/uploads/${params.uploadId}/${params.fileName}` };
    }
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
  viewport.snapshots = new Set(['context', 'sessions', 'selection', 'messages', 'collaboration']);
  viewport.context = context;
  const { server } = await createServer(config, viewport);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); return new Promise(resolve => server.close(resolve)); });
  const url = `http://127.0.0.1:${server.address().port}`;
  const headers = { authorization: `Basic ${Buffer.from(`opencode:${config.token}`).toString('base64')}`, 'content-type': 'application/json', 'x-crew-web-origin-verified': 'true' };
  const post = (path, value, customHeaders = headers) => fetch(url + path, { method: 'POST', headers: customHeaders, body: JSON.stringify(value) });
  return { config, rpc, viewport, commands, uploads, context, url, headers, post };
}

const message = { requestId: 'request-a', text: 'Continue', model: 'openai-codex/gpt-example', reasoning: 'high', attachments: [] };

test('RPC recovers from a failed dial without a close event and ignores the retired socket', async t => {
  const sockets = [];
  t.mock.method(globalThis, 'WebSocket', function () {
    const socket = new EventTarget();
    socket.readyState = WebSocket.CONNECTING;
    socket.send = frame => { socket.request = JSON.parse(frame); };
    socket.close = () => {
      socket.readyState = WebSocket.CLOSING;
      if (socket !== sockets[0]) socket.dispatchEvent(new Event('close'));
    };
    sockets.push(socket);
    queueMicrotask(() => {
      socket.readyState = socket === sockets[0] ? WebSocket.CLOSING : WebSocket.OPEN;
      socket.dispatchEvent(new Event(socket === sockets[0] ? 'error' : 'open'));
    });
    return socket;
  });
  const rpc = new CrewRpc(39400);
  t.after(() => rpc.close());
  await assert.rejects(rpc.connect(), /Crew engine unavailable/);
  await rpc.connect();
  const reply = rpc.call('ReadSessionAuthority', { chatId: 'session-a' });
  sockets[0].dispatchEvent(new Event('close'));
  sockets[1].dispatchEvent(new MessageEvent('message', { data: JSON.stringify({
    id: sockets[1].request.id, ok: { capabilities: ['session.read'] },
  }) }));
  assert.deepEqual(await reply, { capabilities: ['session.read'] });
});

test('deployment session connects and admits messages and controls without legacy chat rows', async t => {
  const f = await fixture(t);
  const watches = new Map();
  const original = f.rpc.call;
  f.rpc.connect = async () => {};
  f.rpc.watch = (method, params, callback) => { watches.set(method, { params, callback }); };
  f.rpc.call = async (method, params) => {
    if (method === 'ReadSessionSelection') return { selection: null };
    if (method === 'ListSessionModels') return [];
    return original(method, params);
  };
  f.viewport.clearProtected();
  await f.viewport.connect();
  assert.equal(watches.has('WatchChats'), false);
  watches.get('WatchSessions').callback([]);
  watches.get('WatchDocMessages').callback({ reset: [], before: null });
   watches.get('WatchCollaboration').callback({ sessions: [{ sessionId: 'session-a', status: 'idle', model: 'openai-codex/gpt-example' }], grants: [{ id: 'private-grant' }], participants: [{ email: 'private@example.com' }] });
  await Promise.resolve();
  assert.equal(f.viewport.chat, undefined);
  assert.equal(f.viewport.state().capabilities.message, true);
   assert.equal(f.viewport.state().session.cwd, '/workspace');
   const browserState = await (await fetch(f.url + '/api/session', { headers: f.headers })).json();
   assert.equal(Object.hasOwn(browserState, 'collaboration'), false);
   assert.equal(Object.hasOwn(browserState, 'grants'), false);
   assert.equal(Object.hasOwn(browserState, 'participants'), false);
   assert.equal(browserState.session.model, 'openai-codex/gpt-example');
   assert.equal(browserState.capabilities.input, true);
  const { model, reasoning, ...defaultMessage } = message;
  assert.equal((await f.post('/api/message', { ...defaultMessage, cwd: '/browser' })).status, 400);
  assert.equal((await f.post('/api/message', defaultMessage)).status, 202);
  const request = [...f.commands.values()][0].command.action.request;
  assert.equal(request.cwd, '/workspace');
  assert.equal(request.model, model);
  assert.equal(request.reasoning, reasoning);
  assert.deepEqual(request.modelOptions, { trusted: true });
  assert.equal((await f.post('/api/interrupt', { requestId: 'stop-a' })).status, 202);
  f.viewport.messages = [{ parts: [{ kind: 'input', requestId: 'input-a', questions: [{ id: 'q', multiSelect: false }] }] }];
  assert.equal((await f.post('/api/input', { requestId: 'answer-a', inputRequestId: 'input-a', answers: [{ questionId: 'q', labels: ['yes'] }] })).status, 202);
  const session = { chatId: 'session-a::session::session-a', deviceId: 'device-a', status: 'working', startedAt: '2026-09-11T10:00:00Z' };
  watches.get('WatchSessions').callback([session]);
  const firstTurn = f.viewport.state().session.turnId;
  assert.ok(firstTurn);
  watches.get('WatchDocMessages').callback({ reset: [{ id: 'message-a', parts: [] }] });
  watches.get('WatchSessions').callback([{ ...session, status: 'awaitingInput' }]);
  assert.equal(f.viewport.state().session.turnId, firstTurn);
  watches.get('WatchSessions').callback([{ ...session, startedAt: '2026-09-11T10:01:00Z' }]);
  assert.notEqual(f.viewport.state().session.turnId, firstTurn);
  assert.equal(f.viewport.state().messages.at(-1).id, 'message-a');
  watches.get('WatchSessions').callback([{ ...session, startedAt: null }]);
  assert.equal(f.viewport.state().session.turnId, null);
});

test('session context binding mismatch clears protected readiness before admission', async t => {
  const f = await fixture(t);
  await f.viewport.readAuthority();
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => {
    const result = await original(method, params);
    return method === 'ReadSessionContext' ? { ...result, authority: { ...result.authority, lifecycleEpoch: 2 } } : result;
  };
  assert.equal((await f.post('/api/message', message)).status, 503);
  assert.equal(f.viewport.context, null);
  assert.equal(f.viewport.state().capabilities.message, false);
  assert.equal(f.commands.size, 0);
});

test('uploads enforce 45000 raw-byte chunks and preserve bytes across boundaries', async t => {
  const f = await fixture(t);
  await assert.rejects(f.rpc.call('UploadChunk', { uploadId: 'oversize', seq: 0, data: Buffer.alloc(45001).toString('base64') }), /45000/);
  for (const size of [44999, 45000, 45001, 90000, 90001, 192 * 1024]) {
    const bytes = Buffer.alloc(size);
    for (let index = 0; index < size; index++) bytes[index] = index % 251;
    const response = await fetch(f.url + '/api/upload', { method: 'POST', headers: { ...f.headers, 'content-type': 'application/octet-stream', 'x-filename': 'boundary.bin' }, body: bytes });
    assert.equal(response.status, 201);
    const metadata = await response.json();
    assert.equal(metadata.size, size);
    const chunks = f.uploads.get(metadata.id);
    assert.equal(chunks.length, Math.ceil(size / 45000));
    assert.ok(chunks.every(chunk => chunk.length <= 45000));
    assert.deepEqual(Buffer.concat(chunks), bytes);
    assert.equal((await f.post('/api/message', { ...message, requestId: `upload-${size}`, attachments: [metadata] })).status, 202);
    const entry = f.commands.get(`crew-web:upload-${size}`);
    assert.ok(entry.command.action.request.prompt.includes(`/workspace/uploads/${metadata.id}/boundary.bin`));
  }
});

test('upload revocation between chunks prevents remaining chunks and commit', async t => {
  const f = await fixture(t);
  const original = f.rpc.call;
  let revoked = false;
  let commits = 0;
  f.rpc.call = async (method, params) => {
    if (method === 'UploadCommit') commits++;
    const result = await original(method, params);
    if (method === 'UploadChunk') revoked = true;
    return method === 'ReadSessionAuthority' && revoked ? { ...result, capabilities: ['session.read'] } : result;
  };
  const response = await fetch(f.url + '/api/upload', { method: 'POST', headers: { ...f.headers, 'x-filename': 'revoked.bin' }, body: Buffer.alloc(45001) });
  assert.equal(response.status, 403);
  assert.equal([...f.uploads.values()][0].length, 1);
  assert.equal(commits, 0);
});

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
  f.viewport.context.harnessSessionId = 'stale-context-native';
  f.viewport.collaboration = { sessions: [{ sessionId: 'session-a', harnessSessionId: 'stale-projected-native' }] };
  assert.equal((await f.post('/api/message', message)).status, 202);
  const entry = [...f.commands.values()][0];
  assert.equal(entry.chatId, 'session-a');
  assert.equal(entry.command.sessionId, 'session-a');
  assert.equal(entry.command.action.request.resume, null);
});

test('browser restores the authorized route after failed admission or rejection and preserves retries', async () => {
  const source = await readFile(new URL('./public/app.js', import.meta.url), 'utf8');
  for (const failure of ['admission', 'rejection', 'outcome', 'response', 'default-outcome', 'default-response', 'edited-response']) {
    const nodes = new Map();
    const node = id => {
      if (!nodes.has(id)) nodes.set(id, {
        value: '', style: {}, dataset: {}, options: [], listeners: {},
        addEventListener(name, listener) { this.listeners[name] = listener; },
        querySelectorAll() { return []; }, replaceChildren(...children) { this.options = children; },
        add(option) { this.options.push(option); }, focus() {},
      });
      return nodes.get(id);
    };
    const calls = [];
    let fail = true;
    let route = 'gpt-example';
    const admitted = new Map();
    const context = createContext({
      document: { getElementById: node, querySelector: node },
      window: { addEventListener() {} },
      ResizeObserver: class { observe() {} },
      EventSource: class { addEventListener() {} },
      crypto: { randomUUID },
      Option: class { constructor(label, value) { this.label = label; this.value = value; } },
      fetch: async (path, options) => {
        if (path === './api/session' || path === './api/models') return new Promise(() => {});
        const body = options.body && JSON.parse(options.body);
        calls.push({ path, body });
        let status = 200;
        let result = {};
        if (path.endsWith('/model-route')) route = body.model;
        else if (path === './api/interrupt') {
          admitted.set(body.requestId, { status: 'applied' });
          if (fail) throw new TypeError('Response lost');
        } else if (path === './api/message') {
          if (admitted.has(body.requestId)) result = {};
          else if (fail && failure === 'admission') status = 503;
          else if (route !== body.model.split('/')[1]) status = 409;
          else {
            admitted.set(body.requestId, { status: fail && failure === 'rejection' ? 'rejected' : 'applied' });
            if (fail && failure.endsWith('response')) throw new TypeError('Response lost');
          }
        } else {
          result = admitted.get(path.split('/').at(-1));
          if (!result) { status = 404; result = {}; }
          else if (fail && failure.endsWith('outcome')) status = 503;
        }
        return new Response(JSON.stringify(result), { status, headers: { 'content-type': 'application/json' } });
      },
    });
    runInContext(source, context);
    runInContext(`state = { connection: 'connected', sandboxId: 'sandbox-a', session: { model: 'openai-codex/gpt-example', reasoning: 'high', status: 'idle' }, capabilities: { message: true }, models: [{}] }; connected = true;`, context);
    node('message').value = 'Continue';
    const defaultSelection = failure.startsWith('default-') || failure === 'edited-response';
    node('model').value = defaultSelection ? 'openai-codex/gpt-example' : 'openai-codex/gpt-other';
    node('reasoning').value = 'high';
    if (!defaultSelection) node('reasoning').listeners.change();
    const submit = () => node('composer').listeners.submit({ preventDefault() {} });
    await submit();
    assert.equal(node('message').value, 'Continue');
    if (failure === 'admission') {
      node('model').value = 'openai-codex/gpt-example';
      await submit();
      assert.equal(node('message').value, 'Continue');
      assert.equal(route, 'gpt-example');
      node('model').value = 'openai-codex/gpt-other';
    }
    fail = false;
    if (defaultSelection) {
      route = 'remote';
      runInContext(`state.session.model = 'openai-codex/remote'; state.session.status = 'working'; renderModels();`, context);
      assert.equal(node('model').value, 'openai-codex/remote');
      if (failure === 'edited-response') node('message').value = 'New draft';
      await submit();
      const messages = calls.filter(call => call.path === './api/message');
      assert.equal(messages.length, 2);
      if (failure === 'edited-response') {
        assert.notEqual(messages[0].body.requestId, messages[1].body.requestId);
        assert.equal(messages[1].body.text, 'New draft');
        assert.equal(messages[1].body.model, 'openai-codex/remote');
        assert.equal(admitted.size, 2);
      } else {
        assert.deepEqual(messages[1].body, messages[0].body);
        assert.equal(admitted.size, 1);
      }
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 1);
    } else if (failure === 'outcome' || failure === 'response') {
      route = 'gpt-example';
      await submit();
      const messages = calls.filter(call => call.path === './api/message');
      assert.equal(messages[0].body.requestId, messages[1].body.requestId);
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 1);
      assert.equal(admitted.size, 1);
    } else if (failure === 'admission') {
      await submit();
      const messages = calls.filter(call => call.path === './api/message');
      assert.equal(messages.length, 3);
      assert.equal(messages[0].body.requestId, messages[2].body.requestId);
      assert.notEqual(messages[0].body.requestId, messages[1].body.requestId);
      assert.equal(route, 'gpt-other');
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 3);
      assert.equal(admitted.size, 1);
    } else {
      node('model').value = 'openai-codex/gpt-example';
      await submit();
      assert.equal(route, 'gpt-example');
      assert.equal(calls.filter(call => call.path.endsWith('/model-route')).length, 2);
    }
    assert.equal(node('message').value, '');
    assert.equal(runInContext('modelDirty', context), false);
    runInContext(`state.session.model = 'openai-codex/remote'; state.session.status = 'working'; renderModels();`, context);
    assert.equal(node('model').value, 'openai-codex/remote');
    node('model').value = 'openai-codex/unsent';
    node('reasoning').listeners.change();
    runInContext(`state.session.model = 'openai-codex/another'; renderModels();`, context);
    assert.equal(node('model').value, 'openai-codex/unsent');
    runInContext(`state.session.id = 'session-a'; state.session.turnId = JSON.stringify(['session-a', 'device-a', '2026-09-11T10:00:00Z']); state.capabilities.interrupt = true; state.messages = [{ id: 'message-a' }];`, context);
    const stop = () => node('stop').listeners.click();
    fail = true;
    await stop();
    runInContext(`state.messages.push({ id: 'message-a2' }); state.session.status = 'awaitingInput';`, context);
    await stop();
    runInContext(`state.session = { ...state.session, status: 'working', turnId: JSON.stringify(['session-a', 'device-a', '2026-09-11T10:01:00Z']) };`, context);
    fail = false;
    await stop();
    const stops = calls.filter(call => call.path === './api/interrupt');
    assert.equal(stops[0].body.requestId, stops[1].body.requestId);
    assert.notEqual(stops[0].body.requestId, stops[2].body.requestId);
    runInContext(`state.session.turnId = null; updateControls();`, context);
    assert.equal(node('stop').disabled, true);
    await stop();
    assert.equal(calls.filter(call => call.path === './api/interrupt').length, 3);
  }
});

test('durable admission remains discoverable before engine queue acknowledgement', async t => {
  const f = await fixture(t);
  const original = f.rpc.call;
  f.rpc.call = async (method, params) => {
    if (method === 'QueueCommand') throw new Error('Engine unavailable');
    return original(method, params);
  };
  assert.equal((await f.post('/api/message', message)).status, 503);
  assert.equal(f.commands.size, 0);
  const outcome = await fetch(f.url + '/api/command/' + message.requestId, { headers: f.headers });
  assert.equal(outcome.status, 200);
  assert.equal((await outcome.json()).status, 'pending');
  f.rpc.call = original;
  f.viewport.live = { status: 'working' };
  assert.equal((await f.post('/api/message', message)).status, 202);
  assert.equal([...f.commands.values()][0].command.action.action, 'start');
  assert.equal(f.commands.size, 1);
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
