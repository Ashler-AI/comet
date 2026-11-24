// The paused-session view against the ways it can fail:
// D1 reasoning text from the tail reaches the browser;
// D2 a paused session shows a turn as still running (a streaming entry or a live turn id);
// D3 a tail for another chat renders;
// D4 a large tail exceeds the live view's page bounds;
// D5 the view offers older history it cannot page;
// D6 a stopped or failed session offers sending, or a running one is served from the tail;
// D7 tool detail for an unknown part leaks another part or fails.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { detachedState, detachedToolDetail } from './detached.mjs';

const chatId = '4ce53995-fab8-4896-9e62-3b23b3a785ac';
const session = { chatId, sandboxId: 'rcs_d7e3291cd0e28391b0cc5962', title: 'board test 14', status: 'paused', model: 'openai-codex/gpt-5.6-terra' };
const user = { id: 'u1', role: 'user', status: 'complete', createdAt: 1, parts: [{ kind: 'text', id: 't0', text: 'count the lines' }] };
const assistant = (status = 'complete') => ({ id: 'a1', role: 'assistant', status, createdAt: 2, parts: [
  { kind: 'reasoning', id: 'r0', text: 'PRIVATE CHAIN OF THOUGHT' },
  { kind: 'tool', id: 'tool-1', call: { kind: 'exec', command: 'wc -l leases.mjs' }, resolved: true, isError: false, output: '308 leases.mjs' },
  { kind: 'text', id: 't1', text: 'Completed: 308 lines.' },
] });
const tail = (messages, extra = {}) => ({ chatId, schemaVersion: 1, messages, totalMessages: messages.length, updatedAt: 1795559124197, ...extra });

test('reasoning text never reaches the browser (D1)', () => {
  const state = detachedState({ tail: tail([user, assistant()]), session });
  assert.equal(JSON.stringify(state).includes('PRIVATE CHAIN OF THOUGHT'), false);
  assert.deepEqual(state.messages[1].parts[0], { id: 'r0', kind: 'thinking', started: true, completed: true });
});

test('a turn the pause cut off reads as interrupted, not running (D2)', () => {
  const state = detachedState({ tail: tail([user, assistant('streaming')]), session });
  assert.equal(state.messages[1].status, 'aborted', 'the app labels aborted entries "Interrupted"');
  assert.equal(state.messages[1].parts[0].completed, true, 'no live thought');
  assert.equal(state.session.turnId, null, 'no turn to stop');
  assert.equal(state.connection, 'paused');
  assert.deepEqual(state.capabilities, { message: true, input: false, interrupt: false });
});

test("another chat's tail never renders (D3)", () => {
  assert.throws(() => detachedState({ tail: { ...tail([user]), chatId: 'other-chat' }, session }), /another chat/);
  assert.throws(() => detachedToolDetail({ tail: { ...tail([assistant()]), chatId: 'other-chat' }, chatId, toolId: 'tool-1' }), /another chat/);
});

test('a large tail keeps the live page bounds (D4)', () => {
  const many = Array.from({ length: 150 }, (_, index) => ({ ...user, id: `u${index}`, parts: [{ kind: 'text', id: 't', text: `message ${index}` }] }));
  const counted = detachedState({ tail: tail(many), session });
  assert.equal(counted.messages.length, 100);
  assert.equal(counted.messages.at(-1).id, 'u149', 'the newest entries are kept');
  const heavy = Array.from({ length: 40 }, (_, index) => ({ ...user, id: `h${index}`, parts: [{ kind: 'text', id: 't', text: 'x'.repeat(30_000) }] }));
  const sized = detachedState({ tail: tail(heavy), session });
  assert.ok(Buffer.byteLength(JSON.stringify(sized.messages)) <= 512 * 1024 + 1024, 'within the page byte budget');
  assert.ok(sized.messages.length < heavy.length);
});

test('the view states what it shows instead of offering a history cursor (D5)', () => {
  const state = detachedState({ tail: tail([user, assistant()], { totalMessages: 210 }), session });
  assert.deepEqual(state.history, { hasOlder: false, before: null });
  assert.equal(state.detached.shown, 2);
  assert.equal(state.detached.total, 210);
  const empty = detachedState({ tail: null, session });
  assert.equal(empty.detached.available, false);
  assert.deepEqual(empty.messages, []);
});

test('only a session that can resume offers sending, and a running one is never served from the tail (D6)', () => {
  for (const status of ['paused', 'resuming', 'starting']) {
    assert.equal(detachedState({ tail: tail([user]), session: { ...session, status } }).capabilities.message, true, status);
  }
  for (const status of ['stopped', 'failed']) {
    assert.equal(detachedState({ tail: tail([user]), session: { ...session, status } }).capabilities.message, false, status);
  }
  for (const status of ['ready', 'agent_running']) {
    assert.throws(() => detachedState({ tail: tail([user]), session: { ...session, status } }), /served live/);
  }
});

test('tool detail answers only for its own part (D7)', () => {
  const source = tail([user, assistant()]);
  assert.deepEqual(detachedToolDetail({ tail: source, chatId, toolId: 'missing' }), { found: false });
  const detail = detachedToolDetail({ tail: source, chatId, toolId: 'tool-1' });
  assert.equal(detail.found, true);
  assert.equal(detail.output, '308 leases.mjs');
  assert.deepEqual(JSON.parse(detail.input), { kind: 'exec', command: 'wc -l leases.mjs' });
  assert.equal(JSON.stringify(detail).includes('PRIVATE CHAIN OF THOUGHT'), false);
});
