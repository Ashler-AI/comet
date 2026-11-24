// The browser viewport's transcript rules against the desktop's own cases, so a session reads the
// same in both. Failure modes:
// W1 a folded group's summary differs from the desktop's line for the same parts ("2 tool calls"
//    instead of "Thought 2 times · Ran 4 commands");
// W2 parts fold differently: thoughts as separate rows, a reply splitting nothing, agent spawns
//    sharing a fold with ordinary tools, a settled group opening by itself;
// W3 a tool chip names a call differently from the desktop;
// W4 a user message folds at a different size than the desktop's.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { messageRows, toolChipContent, toolGroupSummary, userFolds } from './public/transcript.js';

const run = (command, isError = false) => ({ kind: 'tool', call: { kind: 'exec', command }, isError });
const thought = { kind: 'thought' };

test('a group summary is the desktop line for the same parts (W1)', () => {
  // The desktop showed this for the Board's test 9 turn: two thoughts around four commands.
  assert.equal(toolGroupSummary([thought, run('a'), run('b'), thought, run('c'), run('d')]), 'Thought 2 times · Ran 4 commands');
  assert.equal(toolGroupSummary([thought]), 'Thought process');
  // zeron_proto::view's own cases and doc example.
  assert.equal(toolGroupSummary([run('ls'), { kind: 'tool', call: { kind: 'unknown', name: 'Clone' } }]), 'Ran 1 command · called 1 tool');
  const edit = (path) => ({ kind: 'tool', call: { kind: 'editFile', path } });
  assert.equal(toolGroupSummary([run('a'), run('b'), run('c'), edit('x.ts'), edit('y.ts'), edit('x.ts')]), 'Ran 3 commands · edited 2 files');
  assert.equal(toolGroupSummary([run('a', true), { kind: 'tool', call: { kind: 'readFile', path: 'p' } }, { kind: 'tool', call: { kind: 'glob', pattern: '*' } }]),
    'Ran 1 command · read 1 file · searched 1 time · 1 failed');
});

test('parts fold as the desktop folds them (W2)', () => {
  const tool = (id, call = { kind: 'exec', command: id }) => ({ id, kind: 'tool', call, resolved: true });
  const thinking = (id, completed = true) => ({ id, kind: 'thinking', started: true, completed });
  const text = (id) => ({ id, kind: 'text', text: id });
  const rows = messageRows([thinking('r1'), tool('t1'), tool('t2'), thinking('r2'), tool('t3'), text('reply')]);
  assert.deepEqual(rows.map((row) => row.type), ['group', 'part'], 'thoughts ride the tool group; the reply is its own row');
  assert.equal(rows[0].summary, 'Thought 2 times · Ran 3 commands');
  assert.equal(rows[0].autoOpen, false, 'a settled group stays folded');

  const split = messageRows([tool('a'), text('narration'), tool('b')]);
  assert.deepEqual(split.map((row) => row.type), ['group', 'part', 'group'], 'text between tools splits the fold');

  const agent = { kind: 'unknown', name: 'Agent: scan the repo' };
  const genus = messageRows([tool('read', { kind: 'readFile', path: 'a' }), tool('spawn', agent), thinking('r')]);
  assert.deepEqual(genus.map((row) => row.items.length), [1, 1, 1], 'agent spawns fold apart from tools and thoughts');

  const live = messageRows([text('working'), tool('t'), thinking('r', false)], true);
  assert.equal(live.at(-1).autoOpen, true, 'the streaming tail group opens');
  assert.equal(live.at(-1).items.at(-1).live, true);
  assert.equal(messageRows([tool('omp-goal-state')]).length, 0, 'OMP goal bookkeeping is never shown');
});

test('tool chips name calls as the desktop does (W3)', () => {
  assert.deepEqual(toolChipContent({ kind: 'exec', command: 'wc -l\n  leases.mjs' }), ['Run', 'wc -l leases.mjs']);
  assert.deepEqual(toolChipContent({ kind: 'applyPatch' }), ['Patch', 'workspace']);
  assert.deepEqual(toolChipContent({ kind: 'search', pattern: 'crew', path: 'src' }), ['Search', 'crew in src']);
  assert.deepEqual(toolChipContent({ kind: 'todo', items: [{ done: true }, { done: false }] }), ['Todo', '1/2 done']);
  assert.deepEqual(toolChipContent({ kind: 'mcp', server: 'github', tool: 'search' }), ['MCP', 'github · search']);
  assert.deepEqual(toolChipContent({ kind: 'unknown', name: 'Agent: scan the repo' }), ['Agent', 'scan the repo']);
  assert.deepEqual(toolChipContent({ kind: 'unknown', name: 'Agent' }), ['Agent', '']);
  assert.deepEqual(toolChipContent({ kind: 'unknown', name: 'browser_click' }), ['Tool', 'browser_click']);
});

test('a user message folds past five lines or 400 characters (W4)', () => {
  assert.equal(userFolds('one\ntwo\nthree\nfour\nfive'), false);
  assert.equal(userFolds('one\ntwo\nthree\nfour\nfive\nsix'), true);
  assert.equal(userFolds('x'.repeat(400)), false);
  assert.equal(userFolds('x'.repeat(401)), true);
  assert.equal(userFolds('é'.repeat(400)), false, 'characters, not bytes');
});
