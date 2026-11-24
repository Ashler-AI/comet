// Zeron's transcript rules for this browser viewport, so a session reads here as it does in the
// desktop app. Each rule is a port of the desktop's own:
// - a tool chip's label and one-line detail: `zeron_proto::view::tool_chip_content`;
// - a folded group's summary line ("Thought 2 times · Ran 4 commands"): `zeron_proto::view::
//   tool_group_summary` plus the thought segments of crates/ui `transcript::tool_group_summary`;
// - which parts of an assistant message fold into one group: crates/ui `rows_for_entry`;
// - when a user message folds behind "Show more": crates/ui `USER_COLLAPSED_LINES` and
//   `USER_COLLAPSE_CHARS`.
// Pure (no DOM), so `transcript.test.mjs` checks it against the desktop's cases.

export const USER_COLLAPSED_LINES = 5;
export const USER_COLLAPSE_CHARS = 400;

/** Newlines, tabs and runs of whitespace become single spaces, trimmed (`view::single_line`). */
export function singleLine(text) {
  return String(text ?? '').split(/\s+/).filter(Boolean).join(' ');
}

/** A subagent spawn: an `unknown` call named `Agent` or `Agent: <description>` (`ToolCall::is_subagent_spawn`). */
export function isAgentCall(call) {
  return call?.kind === 'unknown' && typeof call.name === 'string' && (call.name === 'Agent' || call.name.startsWith('Agent: '));
}

/** `[label, detail]` for one tool call, as the desktop names it. */
export function toolChipContent(call) {
  const [label, detail] = chip(call ?? {});
  return [label, singleLine(detail)];
}

function chip(call) {
  switch (call.kind) {
    case 'exec': return ['Run', call.command];
    case 'readFile': return ['Read', call.path];
    case 'writeFile': return ['Write', call.path];
    case 'editFile': return ['Edit', call.path];
    case 'applyPatch': return ['Patch', call.path || 'workspace'];
    case 'search': return ['Search', call.path ? `${call.pattern} in ${call.path}` : call.pattern];
    case 'glob': return ['Glob', call.pattern];
    case 'webFetch': return ['Fetch', call.url];
    case 'webSearch': return ['Web', call.query];
    case 'todo': {
      const items = Array.isArray(call.items) ? call.items : [];
      return ['Todo', `${items.filter((item) => item.done).length}/${items.length} done`];
    }
    case 'mcp': return ['MCP', `${call.server} · ${call.tool}`];
    default: {
      const name = typeof call.name === 'string' ? call.name : String(call.kind ?? 'tool');
      if (name.startsWith('Agent: ')) return ['Agent', name.slice('Agent: '.length)];
      if (name === 'Agent') return ['Agent', ''];
      return ['Tool', name];
    }
  }
}

const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

/**
 * The collapsed line of a group of `items` (`{ kind: 'thought' }` or `{ kind: 'tool', call, isError }`):
 * thought segments first, then what the tools did, then failures; only the first letter is capitalized.
 */
export function toolGroupSummary(items) {
  const thoughts = items.filter((item) => item.kind === 'thought').length;
  const tools = items.filter((item) => item.kind === 'tool');
  const segments = [];
  if (thoughts === 1) segments.push('thought process');
  else if (thoughts > 1) segments.push(`thought ${thoughts} times`);
  if (tools.length) segments.push(callsSummary(tools));
  const summary = segments.join(' · ');
  return summary.charAt(0).toUpperCase() + summary.slice(1);
}

function callsSummary(tools) {
  let commands = 0, reads = 0, searches = 0, fetches = 0, todos = 0, other = 0, failed = 0;
  const edited = [];
  for (const { call, isError } of tools) {
    if (isError) failed += 1;
    switch (call?.kind) {
      case 'exec': commands += 1; break;
      case 'writeFile':
      case 'editFile': if (!edited.includes(call.path)) edited.push(call.path); break;
      case 'applyPatch': {
        const path = call.path || 'patch';
        if (!edited.includes(path)) edited.push(path);
        break;
      }
      case 'readFile': reads += 1; break;
      case 'search':
      case 'glob':
      case 'webSearch': searches += 1; break;
      case 'webFetch': fetches += 1; break;
      case 'todo': todos += 1; break;
      default: other += 1;
    }
  }
  const segments = [];
  if (commands) segments.push(`ran ${plural(commands, 'command', 'commands')}`);
  if (edited.length) segments.push(`edited ${plural(edited.length, 'file', 'files')}`);
  if (reads) segments.push(`read ${plural(reads, 'file', 'files')}`);
  if (searches) segments.push(`searched ${plural(searches, 'time', 'times')}`);
  if (fetches) segments.push(`fetched ${plural(fetches, 'page', 'pages')}`);
  if (todos) segments.push('updated todos');
  if (other) segments.push(`called ${plural(other, 'tool', 'tools')}`);
  if (!segments.length) segments.push(plural(tools.length, 'tool', 'tools'));
  if (failed) segments.push(`${failed} failed`);
  // Capitalized on its own, as zeron_proto returns it: after a thought segment it still reads "Ran".
  const summary = segments.join(' · ');
  return summary.charAt(0).toUpperCase() + summary.slice(1);
}

/**
 * An assistant message's parts as the rows the desktop shows: consecutive tool calls and thoughts
 * fold into one `{ type: 'group', items, summary, autoOpen }`; agent spawns fold only with each
 * other; every other part is its own `{ type: 'part', part }`. A group opens by itself only while
 * it is the streaming tail of the message. `omp-goal-state` is OMP bookkeeping, never shown.
 */
export function messageRows(parts, streaming = false) {
  const rows = [];
  let group = [];
  let groupEnd = -1;
  const lastIndex = parts.length - 1;
  const flush = () => {
    if (!group.length) return;
    rows.push({ type: 'group', items: group, summary: toolGroupSummary(group), autoOpen: streaming && groupEnd === lastIndex });
    group = [];
  };
  parts.forEach((part, index) => {
    if (part.kind === 'tool') {
      if (part.id === 'omp-goal-state') return;
      const item = { kind: 'tool', call: part.call, isError: Boolean(part.isError), part };
      if (group.length && isAgentItem(group[0]) !== isAgentItem(item)) flush();
      group.push(item);
      groupEnd = index;
      return;
    }
    if (part.kind === 'thinking') {
      if (!part.started) return;
      if (group.length && isAgentItem(group[0])) flush();
      group.push({ kind: 'thought', part, live: streaming && index === lastIndex && !part.completed });
      groupEnd = index;
      return;
    }
    flush();
    rows.push({ type: 'part', part });
  });
  flush();
  return rows;
}

function isAgentItem(item) {
  return item.kind === 'tool' && isAgentCall(item.call);
}

/** Whether a user message folds behind "Show more" (more than 5 lines or 400 characters). */
export function userFolds(text) {
  const value = String(text ?? '');
  return value.split('\n').length > USER_COLLAPSED_LINES || [...value].length > USER_COLLAPSE_CHARS;
}
