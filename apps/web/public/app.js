const $ = (id) => document.getElementById(id);
const ui = Object.fromEntries(['session-title', 'connection', 'empty', 'empty-prompt', 'transcript', 'older', 'pending-input', 'activity', 'latest', 'composer', 'message', 'model', 'reasoning', 'attach', 'file-input', 'attachments', 'send', 'stop', 'cwd', 'branch', 'error', 'error-text'].map((id) => [id, $(id)]));
let state;
let connected = false;
let sending = false;
let stopping = false;
let uploads = [];
let modelDirty = false;
let modelRevision = 0;
let modelCatalog;
let modelSignature = '';
let inputSignature = '';
let historyBefore;
let historyAnchor;
let historyEpoch = 0;
let loadingHistory = false;
let frameVersion = 0;
const retainedMessages = new Map();
const messageNodes = new Map();
const toolDetails = new Map();
function userText(text) {
  const root = element('div');
  const markers = Array.from(text.matchAll(/\n\nAttached (?:images|files|text) \(local files[^\n]*\):\r?\n/gi));
  const refs = [];
  let bodyEnd = text.length;
  for (let index = 0; index < markers.length; index++) {
    const marker = markers[index];
    const paths = text.slice(marker.index + marker[0].length, markers[index + 1]?.index ?? text.length).split('\n').map((line) => line.match(/^\s*-\s+(.+)$/)?.[1]?.trim()).filter(Boolean);
    if (paths.length) { bodyEnd = Math.min(bodyEnd, marker.index); refs.push(...paths); }
  }
  let body = text.slice(0, bodyEnd).trimEnd();
  if (refs.length && ['See the attached image(s).', 'See the attached file(s).', 'See the attached text.'].includes(body.trim())) body = '';
  if (body) root.append(element('div', 'markdown', body));
  if (refs.length) {
    const strip = element('div', 'attachments');
    for (const path of refs) {
      const name = path.split(/[\\/]/).at(-1).replace(/^[a-f\d]{8}-/i, '') || 'Attachment';
      const chip = element('span', 'attachment', name); chip.title = path; strip.append(chip);
    }
    root.append(strip);
  }
  return root;
}
const commandIds = new Map();
const unresolvedMessages = new Map();
const pendingCommands = new Map();
const inputDrafts = new Map();
const terminalCommandStatuses = new Set(['rejected', 'expired', 'superseded', 'cancelled']);

function renderActivity() {
  const statuses = { working: 'Composing…', awaitingInput: 'Awaiting your answer', errored: 'Session encountered an error', idle: '' };
  const pending = pendingCommands.values().next().value;
  ui.activity.textContent = pending || (statuses[state?.session.status] ?? state?.session.status ?? '');
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = String(text);
  return node;
}
function button(text, action, className = 'text-button') {
  const node = element('button', className, text);
  node.type = 'button';
  node.addEventListener('click', action);
  return node;
}
function showError(error) {
  ui['error-text'].textContent = error instanceof Error ? error.message : String(error);
  ui.error.hidden = false;
}
$('dismiss-error').addEventListener('click', () => { ui.error.hidden = true; });
async function api(path, options = {}) {
  const response = await fetch(path.startsWith('/') ? path : `./api/${path}`, { credentials: 'same-origin', cache: 'no-store', ...options });
  const contentType = response.headers.get('content-type') || '';
  const payload = contentType.includes('application/json') ? await response.json() : null;
  if (!response.ok) throw Object.assign(new Error(payload?.error?.message || payload?.error || payload?.message || `Request failed (${response.status}). Please try again.`), { status: response.status });
  if (payload === null) throw new Error('Crew returned an unexpected response. Check the connection and try again.');
  return payload;
}
async function command(path, payload, scope = '') {
  const key = `${path}:${JSON.stringify(payload)}${scope}`;
  let requestId = commandIds.get(key);
  if (!requestId) {
    requestId = crypto.randomUUID();
    commandIds.set(key, requestId);
  }
  const label = path === 'message' ? 'Message' : path === 'input' ? 'Answer' : 'Stop request';
  let admitted = false;
  try {
    await api(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ ...payload, requestId }) });
    admitted = true;
    pendingCommands.set(requestId, `${label} queued · awaiting execution`);
    renderActivity();
    while (true) {
      const outcome = await api(`command/${encodeURIComponent(requestId)}`);
      if (outcome.status === 'applied') {
        commandIds.delete(key);
        return outcome;
      }
      if (terminalCommandStatuses.has(outcome.status)) {
        commandIds.delete(key);
        throw new Error(`${label} ${outcome.status}${outcome.resolution ? `: ${outcome.resolution}` : '.'} Your content has been kept. You can retry.`);
      }
      if (outcome.status !== 'pending') throw new Error('Crew returned an unknown command outcome.');
      await new Promise((resolve) => setTimeout(resolve, 750));
    }
  } catch (error) {
    if (admitted && commandIds.has(key)) throw new Error(`${label} outcome could not be confirmed: ${error.message} Your content has been kept. Retry to check the same request.`);
    throw error;
  } finally {
    pendingCommands.delete(requestId);
    renderActivity();
  }
}
function running() { return state?.session.status === 'working' || state?.session.status === 'awaitingInput'; }
function allowed(capability) { return connected && state?.connection === 'connected' && state?.capabilities?.[capability] === true; }
function updateControls() {
  const ready = allowed('message');
  ui.message.disabled = !state;
  ui.attach.disabled = !ready || sending;
  ui.model.disabled = !ready || sending || running() || !availableModels().length;
  ui.reasoning.disabled = !ready || sending || running() || ui.reasoning.options.length < 2;
  ui.send.disabled = !ready || sending || uploads.some((file) => file.uploading) || (!ui.message.value.trim() && !uploads.length);
  ui.send.title = ui.send.ariaLabel = running() ? 'Steer agent' : 'Send message';
  ui.message.placeholder = running() ? 'Steer this agent…' : 'Message Crew…';
  ui.stop.hidden = !running();
  ui.stop.disabled = !allowed('interrupt') || !state?.session.turnId || stopping;
  ui.stop.title = ui.stop.ariaLabel = stopping ? 'Stop requested…' : 'Stop agent';
  for (const submit of ui['pending-input'].querySelectorAll('button[type="submit"]')) submit.disabled = !allowed('input') || submit.dataset.submitting === 'true';
}
function connection(value) {
  connected = value;
  const live = value && state?.connection === 'connected';
  ui.connection.dataset.state = live ? 'connected' : 'disconnected';
  ui.connection.textContent = live ? 'Connected' : state ? 'Reconnecting…' : 'Connecting…';
  updateControls();
}

// Markdown is constructed entirely from DOM text nodes. Raw HTML and remote images
// stay inert; message content can never create executable markup or load trackers.
function inline(parent, text) {
  const pattern = /(`+)([^`\n]+)\1|\*\*([^*\n]+)\*\*|__([^_\n]+)__|\*([^*\n]+)\*|~~([^~\n]+)~~|\[([^\]\n]+)\]\(([^\s)]+)\)/g;
  let offset = 0;
  for (const match of text.matchAll(pattern)) {
    parent.append(document.createTextNode(text.slice(offset, match.index)));
    if (match[1]) parent.append(element('code', '', match[2]));
    else if (match[3] || match[4]) parent.append(element('strong', '', match[3] || match[4]));
    else if (match[5]) parent.append(element('em', '', match[5]));
    else if (match[6]) parent.append(element('del', '', match[6]));
    else {
      let url;
      try { url = new URL(match[8]); } catch { /* Non-web references stay readable text. */ }
      if (url && ['https:', 'http:', 'mailto:'].includes(url.protocol)) {
        const link = element('a', '', match[7]);
        link.href = url.href;
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
        parent.append(link);
      } else parent.append(document.createTextNode(match[0]));
    }
    offset = match.index + match[0].length;
  }
  parent.append(document.createTextNode(text.slice(offset)));
}
function markdown(text) {
  const root = element('div', 'markdown');
  const lines = text.replace(/\r\n/g, '\n').split('\n');
  const isBoundary = (line) => /^\s*$|^ {0,3}(?:`{3,}|~{3,}|#{1,6}\s|>\s?|[-*+]\s|\d+[.)]\s|(?:[-*_]\s*){3,}$)/.test(line);
  for (let i = 0; i < lines.length;) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    const fence = line.match(/^ {0,3}(`{3,}|~{3,})(.*)$/);
    if (fence) {
      const content = [];
      i++;
      const close = new RegExp(`^ {0,3}${fence[1][0]}{${fence[1].length},}\\s*$`);
      while (i < lines.length && !close.test(lines[i])) content.push(lines[i++]);
      if (i < lines.length) i++;
      const pre = element('pre');
      if (fence[2].trim()) pre.append(element('span', 'code-caption', fence[2].trim()));
      pre.append(element('code', '', content.join('\n')));
      root.append(pre);
      continue;
    }
    const heading = line.match(/^ {0,3}(#{1,6})\s+(.+?)\s*#*$/);
    if (heading) { const node = element(`h${heading[1].length}`); inline(node, heading[2]); root.append(node); i++; continue; }
    if (/^ {0,3}(?:[-*_]\s*){3,}$/.test(line)) { root.append(element('hr')); i++; continue; }
    if (/^ {0,3}>/.test(line)) {
      const content = [];
      while (i < lines.length && /^ {0,3}>/.test(lines[i])) content.push(lines[i++].replace(/^ {0,3}>\s?/, ''));
      const quote = element('blockquote'); quote.append(markdown(content.join('\n'))); root.append(quote); continue;
    }
    const list = line.match(/^\s*([-*+]|\d+[.)])\s+(.+)$/);
    if (list) {
      const ordered = /^\d/.test(list[1]);
      const node = element(ordered ? 'ol' : 'ul');
      if (ordered) node.start = Number.parseInt(list[1], 10);
      while (i < lines.length) {
        const item = lines[i].match(/^\s*([-*+]|\d+[.)])\s+(.+)$/);
        if (!item || /^\d/.test(item[1]) !== ordered) break;
        const li = element('li'); inline(li, item[2]); node.append(li); i++;
      }
      root.append(node); continue;
    }
    if (line.includes('|') && i + 1 < lines.length && /^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(lines[i + 1])) {
      const table = element('table');
      const cells = (value) => value.trim().replace(/^\|/, '').replace(/\|$/, '').split('|');
      const addRow = (value, tag, parent) => { const row = element('tr'); for (const cell of cells(value)) { const node = element(tag); inline(node, cell.trim()); row.append(node); } parent.append(row); };
      const head = element('thead'); addRow(line, 'th', head); table.append(head); i += 2;
      const body = element('tbody');
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) addRow(lines[i++], 'td', body);
      table.append(body); const wrap = element('div', 'table-wrap'); wrap.append(table); root.append(wrap); continue;
    }
    const paragraph = [lines[i++]];
    while (i < lines.length && !isBoundary(lines[i])) {
      if (lines[i].includes('|') && i + 1 < lines.length && /^\s*\|?\s*:?-{3,}/.test(lines[i + 1])) break;
      paragraph.push(lines[i++]);
    }
    const node = element('p'); inline(node, paragraph.join('\n')); root.append(node);
  }
  return root;
}
function toolLabel(call) {
  switch (call.kind) {
    case 'exec': return ['Run', call.command];
    case 'readFile': return ['Read', call.path];
    case 'writeFile': return ['Write', call.path];
    case 'editFile': return ['Edit', call.path];
    case 'applyPatch': return ['Patch', call.path || 'workspace'];
    case 'search': return ['Search', `${call.pattern}${call.path ? ` in ${call.path}` : ''}`];
    case 'glob': return ['Glob', call.pattern];
    case 'webFetch': return ['Fetch', call.url];
    case 'webSearch': return ['Web', call.query];
    case 'todo': return ['Todo', `${call.items.filter((item) => item.done).length}/${call.items.length} done`];
    case 'agent': return ['Agents', call.agents.map((agent) => `${agent.id} · ${agent.role} · ${agent.status}`).join(', ')];
    case 'mcp': return ['MCP', `${call.server} · ${call.tool}`];
    default: return ['Tool', call.name || call.kind];
  }
}
function paintToolDetail(container, detail, part) {
  container.replaceChildren();
  if (detail?.error) container.append(element('p', '', detail.error));
  else if (detail) {
    if (detail.input != null) { container.append(element('h4', '', 'Input'), element('pre', '', detail.input)); }
    if (detail.output != null) { container.append(element('h4', '', detail.isError ? 'Error output' : 'Output'), element('pre', '', detail.output)); }
    if (detail.input == null && detail.output == null) container.append(element('p', '', detail.found === false ? 'Tool detail is not available in this session’s journal.' : 'No captured output.'));
  } else container.append(element('p', '', 'Loading tool detail…'));
  if (part.call.kind === 'todo') for (const item of part.call.items) container.append(element('p', '', `${item.done ? 'Done' : 'Open'} · ${item.text}`));
  if (part.call.kind === 'agent') for (const agent of part.call.agents) container.append(element('p', '', `${agent.id} · ${agent.role} · ${agent.status}${agent.model ? ` · ${agent.model}` : ''}`));
  if (detail) container.append(button('Refresh detail', () => loadToolDetail(container, part, true)));
}
async function loadToolDetail(container, part, force = false) {
  const cached = toolDetails.get(part.id);
  if (!force && cached && cached.resolved === part.resolved && !cached.error) { paintToolDetail(container, cached, part); return; }
  paintToolDetail(container, null, part);
  try {
    const detail = await api(`tool/${encodeURIComponent(part.id)}`);
    if (!container.isConnected) return;
    toolDetails.set(part.id, detail);
    paintToolDetail(container, detail, part);
  } catch (error) { if (container.isConnected) paintToolDetail(container, { error: error.message }, part); }
}
function renderTool(part) {
  const node = element('details', `tool${part.isError ? ' failed' : ''}`);
  node.dataset.fold = `tool:${part.id}`;
  const summary = element('summary');
  const [label, description] = toolLabel(part.call);
  summary.append(element('span', 'tool-icon', '◇'), element('span', 'tool-label', label), element('span', 'tool-description', description));
  summary.title = `${label} ${description}`;
  if (!part.resolved) summary.append(element('span', 'tool-state', 'Pending'));
  else if (part.isError) summary.append(element('span', 'tool-state', 'Failed'));
  const detail = element('div', 'tool-detail');
  node.append(summary, detail);
  node.addEventListener('toggle', () => { if (node.open) loadToolDetail(detail, part); });
  return node;
}
function renderMessage(message) {
  const node = element('article', `message ${message.role}${message.continuationOf ? ' continuation' : ''}`);
  node.dataset.messageId = message.id;
  node.ariaLabel = message.role === 'user' ? 'You' : message.role === 'assistant' ? 'Crew' : 'System';
  if (message.peerMessage && !message.continuationOf && message.role === 'user') {
    const peer = element('details', 'peer-message'); peer.dataset.fold = `peer:${message.id}`;
    peer.append(element('summary', '', 'Message from another agent'), element('pre', '', message.parts.filter((part) => part.kind === 'text' || part.kind === 'textWindow').map((part) => part.text).join('\n\n')));
    node.append(peer); return node;
  }
  let group = [];
  const flush = () => {
    if (!group.length) return;
    const wrap = element('details', 'tool-group');
    wrap.dataset.fold = `group:${group[0].id}`;
    wrap.open = message.status === 'streaming';
    const failed = group.filter((part) => part.isError).length;
    wrap.append(element('summary', '', `${group.length} tool ${group.length === 1 ? 'call' : 'calls'}${failed ? ` · ${failed} failed` : ''}`));
    const list = element('div', 'tool-list');
    for (const part of group) list.append(renderTool(part));
    wrap.append(list); node.append(wrap); group = [];
  };
  for (const part of message.parts) {
    if (part.kind === 'tool') { if (part.id !== 'omp-goal-state') group.push(part); continue; }
    flush();
    if (part.kind === 'text' || part.kind === 'textWindow') {
      if (part.kind === 'textWindow') node.append(element('p', 'truncation', `Earlier text omitted (${Number(part.omitted_prefix_bytes || 0).toLocaleString()} bytes).`));
      if (message.role === 'user') node.append(userText(part.text));
      else node.append(markdown(part.text));
    } else if (part.kind === 'thinking' && part.started) {
      const active = !part.completed && message.status === 'streaming';
      node.append(element('p', `thinking${active ? ' active' : ''}`, part.completed ? 'Thought process completed' : active ? 'Thinking…' : 'Thought process ended'));
    } else if (part.kind === 'input') {
      node.append(element('p', 'input-chip', part.resolved ? `Answered · ${part.questions[0]?.header || 'Agent question'}` : 'Awaiting your answer…'));
    } else if (part.kind === 'error') node.append(element('div', 'part-error', part.message));
  }
  flush();
  const meta = element('div', 'message-meta');
  if (message.status === 'queued' || message.status === 'steered') meta.append(element('span', 'delivery', message.status === 'queued' ? 'Queued for the next turn' : 'Steered'));
  if (message.status === 'aborted') meta.append(element('span', '', 'Interrupted'));
  if (message.createdAt && message.status !== 'streaming') {
    const date = new Date(message.createdAt);
    const time = element('time', '', date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }));
    time.dateTime = date.toISOString(); time.title = date.toLocaleString(); meta.append(time);
  }
  if (meta.childNodes.length) node.append(meta);
  return node;
}
function nearBottom() { return document.documentElement.scrollHeight - window.innerHeight - window.scrollY < 100; }
function scrollLatest() { window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'instant' }); }
function renderTranscript(messages) {
  const keepBottom = nearBottom();
  const present = new Set();
  let previous = null;
  for (const message of messages) {
    present.add(message.id);
    const signature = JSON.stringify(message);
    let cached = messageNodes.get(message.id);
    if (!cached || cached.signature !== signature) {
      const folds = new Map(Array.from(cached?.node.querySelectorAll('details[data-fold]') || [], (node) => [node.dataset.fold, node.open]));
      const focusedFold = document.activeElement?.closest('details[data-fold]')?.dataset.fold;
      const node = renderMessage(message);
      for (const detail of node.querySelectorAll('details[data-fold]')) if (folds.has(detail.dataset.fold)) detail.open = folds.get(detail.dataset.fold);
      if (cached) cached.node.replaceWith(node);
      cached = { signature, node }; messageNodes.set(message.id, cached);
      if (focusedFold) Array.from(node.querySelectorAll('details[data-fold]')).find((detail) => detail.dataset.fold === focusedFold)?.querySelector('summary')?.focus({ preventScroll: true });
    }
    const next = previous ? previous.nextSibling : ui.transcript.firstChild;
    if (next !== cached.node) ui.transcript.insertBefore(cached.node, next);
    previous = cached.node;
  }
  for (const [id, entry] of messageNodes) if (!present.has(id)) { entry.node.remove(); messageNodes.delete(id); }
  ui.transcript.ariaBusy = 'false';
  if (keepBottom) requestAnimationFrame(scrollLatest);
}
function setReasoning(selected) {
  const models = availableModels();
  const levels = models.find((model) => model.id === ui.model.value)?.reasoningLevels || [];
  ui.reasoning.replaceChildren(new Option('Default', ''));
  for (const level of levels) ui.reasoning.add(new Option(level === 'xhigh' ? 'X-High' : level[0].toUpperCase() + level.slice(1), level));
  if (selected && !levels.includes(selected)) ui.reasoning.add(new Option(selected, selected));
  ui.reasoning.value = selected || '';
}
function availableModels() { return modelCatalog || state?.models || []; }
function renderModels() {
  const selected = modelDirty ? ui.model.value : state.session.model;
  const reasoning = modelDirty ? ui.reasoning.value : state.session.reasoning;
  const models = availableModels();
  const signature = JSON.stringify([models, selected, reasoning]);
  if (signature === modelSignature) return;
  modelSignature = signature;
  ui.model.replaceChildren();
  for (const model of models) ui.model.add(new Option(model.label, model.id));
  if (selected && !Array.from(ui.model.options).some((option) => option.value === selected)) ui.model.add(new Option(selected, selected));
  if (!ui.model.options.length) ui.model.add(new Option('Session model', ''));
  ui.model.value = selected || ui.model.options[0].value;
  setReasoning(reasoning);
}
function renderInputs(messages) {
  const pending = messages.flatMap((message) => message.parts).filter((part) => part.kind === 'input' && !part.resolved);
  const unique = Array.from(new Map(pending.map((part) => [part.requestId, part])).values());
  const signature = JSON.stringify(unique);
  if (signature === inputSignature) return;
  inputSignature = signature;
  ui['pending-input'].replaceChildren();
  ui['pending-input'].hidden = !unique.length;
  for (const part of unique) {
    const form = element('form');
    const fields = [];
    for (const question of part.questions) {
      const field = element('fieldset', 'question');
      field.append(element('legend', '', question.header), element('p', '', question.question));
      const controls = [];
      for (const option of question.options) {
        const label = element('label', 'question-option');
        const input = element('input'); input.type = question.multiSelect ? 'checkbox' : 'radio'; input.name = question.id; input.value = option;
        label.append(input, element('span', '', option)); field.append(label); controls.push(input);
      }
      const custom = element('textarea', 'question-text'); custom.rows = 1; custom.placeholder = question.options.length ? 'Or write an answer…' : 'Your answer…'; custom.ariaLabel = `${question.header}: your answer`;
      if (!question.multiSelect) {
        custom.addEventListener('input', () => { if (custom.value.trim()) for (const control of controls) control.checked = false; });
        for (const control of controls) control.addEventListener('change', () => { if (control.checked) custom.value = ''; });
      }
      const draftKey = `${part.requestId}:${question.id}`;
      const draft = inputDrafts.get(draftKey);
      if (draft) {
        custom.value = draft.text;
        for (const control of controls) control.checked = draft.labels.includes(control.value);
      }
      const saveDraft = () => inputDrafts.set(draftKey, { text: custom.value, labels: controls.filter((control) => control.checked).map((control) => control.value) });
      custom.addEventListener('input', saveDraft);
      for (const control of controls) control.addEventListener('change', saveDraft);
      field.append(custom); form.append(field); fields.push({ question, controls, custom });
    }
    const submit = element('button', 'answer-button', 'Submit answer'); submit.type = 'submit'; form.append(submit);
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      if (!allowed('input') || submit.dataset.submitting === 'true') return;
      const answers = fields.map(({ question, controls, custom }) => ({ questionId: question.id, labels: [...controls.filter((control) => control.checked).map((control) => control.value), ...(custom.value.trim() ? [custom.value.trim()] : [])] }));
      const missing = answers.findIndex((answer) => !answer.labels.length);
      if (missing >= 0) { showError('Please answer each question before submitting.'); fields[missing].custom.focus(); return; }
      submit.dataset.submitting = 'true'; submit.disabled = true;
      submit.textContent = 'Submitting answer…';
      try {
        await command('input', { inputRequestId: part.requestId, answers });
        for (const question of part.questions) inputDrafts.delete(`${part.requestId}:${question.id}`);
        submit.textContent = 'Answer submitted';
        for (const field of form.querySelectorAll('fieldset')) field.disabled = true;
      } catch (error) { submit.dataset.submitting = 'false'; submit.textContent = 'Submit answer'; showError(error); }
      updateControls();
    });
    ui['pending-input'].append(form);
  }
}
function mergeTranscript(next) {
  const previous = state?.messages || [];
  const nextIds = new Set(next.messages.map((message) => message.id));
  const firstOverlap = previous.findIndex((message) => nextIds.has(message.id));
  const oldBefore = state?.history?.before ?? 0;
  const nextBefore = next.history?.hasOlder ? next.history.before : null;
  const advanced = (nextBefore ?? 0) > oldBefore;
  const reset = !state || next.transcriptReset === true || next.session.id !== state.session.id
    || (nextBefore ?? 0) < oldBefore || (previous.length > 0 && firstOverlap < 0 && !advanced);
  if (reset) {
    retainedMessages.clear();
    toolDetails.clear();
    historyBefore = nextBefore;
    historyAnchor = next.messages[0]?.id;
    historyEpoch++;
  } else {
    // A growing raw-list cursor evicts only the prefix before the first shared
    // entry. Missing entries inside the overlapping window are real removals.
    for (let index = 0; index < previous.length; index++) {
      const message = previous[index];
      const evicted = advanced && (firstOverlap < 0 || index < firstOverlap);
      if (!nextIds.has(message.id) && !evicted) retainedMessages.delete(message.id);
    }
    if (advanced && firstOverlap < 0) {
      // Updates missed while disconnected can leave a gap above the new window.
      // Reopen pagination at that boundary, even if older history was exhausted.
      historyBefore = nextBefore;
      historyAnchor = next.messages[0]?.id;
      historyEpoch++;
    }
  }
  for (const message of next.messages) retainedMessages.delete(message.id);
  for (const message of next.messages) retainedMessages.set(message.id, message);
  return Array.from(retainedMessages.values());
}

function receive(next) {
  if (!next?.session || !Array.isArray(next.messages) || !Array.isArray(next.models)) { showError('Crew sent an invalid session update.'); connection(false); return; }
  const entries = mergeTranscript(next);
  state = next;
  document.title = `${state.session.title || 'Session'} · Crew`;
  ui['session-title'].textContent = state.session.title || '';
  ui.cwd.textContent = state.session.cwd || ''; ui.cwd.title = state.session.cwd || 'Assigned working directory';
  ui.branch.textContent = state.session.branch ? `⑂ ${state.session.branch}` : ''; ui.branch.title = state.session.branch || '';
  const project = state.session.cwd?.split('/').filter(Boolean).at(-1) || 'this project';
  ui['empty-prompt'].textContent = `Send a message to get started in ${project}.`;
  if (state.transcriptReset === true) { modelCatalog = undefined; modelSignature = ''; }
  ui.empty.hidden = entries.length > 0;
  renderTranscript(entries);
  renderModels();
  renderInputs(entries);
  ui.older.hidden = historyBefore == null;
  renderActivity();
  connection(connected);
}
function resizeComposer() {
  ui.message.style.height = 'auto';
  ui.message.style.height = `${Math.min(ui.message.scrollHeight, 240)}px`;
}
ui.message.addEventListener('input', () => { resizeComposer(); updateControls(); });
ui.message.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !event.altKey && !event.ctrlKey && !event.metaKey) {
    event.preventDefault();
    if (!ui.send.disabled) ui.composer.requestSubmit();
  }
});
ui.model.addEventListener('change', () => { modelDirty = true; modelRevision++; setReasoning(''); updateControls(); });
ui.reasoning.addEventListener('change', () => { modelDirty = true; modelRevision++; });
ui.composer.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (ui.send.disabled) return;
  const text = ui.message.value;
  const submittedModelRevision = modelRevision;
  const submitted = uploads.slice();
  let payload = { text, attachments: submitted.map((file) => file.metadata) };
  if (ui.model.value) payload.model = ui.model.value;
  payload.reasoning = ui.reasoning.value || null;
  const draftKey = JSON.stringify([text, payload.attachments, submittedModelRevision, modelDirty ? [payload.model, payload.reasoning] : null]);
  const currentPayload = payload;
  payload = unresolvedMessages.get(draftKey) || payload;
  sending = true; updateControls();
  try {
    const requestId = commandIds.get(`message:${JSON.stringify(payload)}`);
    let retrying = false;
    if (requestId) {
      try {
        await api(`command/${encodeURIComponent(requestId)}`);
        retrying = true;
      } catch (error) {
        if (error.status !== 404) throw error;
        unresolvedMessages.delete(draftKey);
        payload = currentPayload;
      }
    }
    if (!retrying && running() && (payload.model !== state.session.model || payload.reasoning !== (state.session.reasoning ?? null))) throw new Error('Stop the current turn before changing model or reasoning. Your message has been kept.');
    if (!retrying && !running() && payload.model) {
      const separator = payload.model.indexOf('/');
      const providerId = payload.model.slice(0, separator);
      const provider = providerId === 'openai-codex' ? 'openai' : providerId === 'anthropic' ? 'anthropic' : null;
      if (!provider || separator < 0) throw new Error('This model is not supported by the session’s inference route.');
      if (!state.sandboxId) throw new Error('The sandbox identity is unavailable. Reconnect before changing models.');
      await api(`/sessions/${encodeURIComponent(state.sandboxId)}/opencode/api/model-route`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ provider, model: payload.model.slice(separator + 1) }) });
    }
    unresolvedMessages.set(draftKey, payload);
    await command('message', payload);
    if (modelRevision === submittedModelRevision && ui.message.value === text) modelDirty = false;
    if (ui.message.value === text) ui.message.value = '';
    uploads = uploads.filter((file) => !submitted.includes(file));
    renderAttachments(); resizeComposer();
    ui.message.focus();
  } catch (error) { showError(error); }
  finally {
    if (!commandIds.has(`message:${JSON.stringify(payload)}`)) unresolvedMessages.delete(draftKey);
    sending = false; updateControls();
  }
});
ui.stop.addEventListener('click', async () => {
  if (!allowed('interrupt') || !running() || !state.session.turnId || stopping) return;
  stopping = true; updateControls();
  const turnId = state.session.turnId;
  try { await command('interrupt', { turnId }, turnId); }
  catch (error) { showError(error); }
  finally { stopping = false; updateControls(); }
});
function renderAttachments() {
  ui.attachments.replaceChildren(); ui.attachments.hidden = !uploads.length;
  for (const file of uploads) {
    const chip = element('div', `attachment${file.uploading ? ' uploading' : ''}`);
    const label = element('span', '', `${file.name}${file.uploading ? ' · Uploading…' : ''}`); label.title = file.name; chip.append(label);
    if (!file.uploading) {
      const remove = button('×', () => { uploads = uploads.filter((item) => item !== file); renderAttachments(); updateControls(); }, '');
      remove.ariaLabel = `Remove ${file.name}`; remove.disabled = sending; chip.append(remove);
    }
    ui.attachments.append(chip);
  }
}
ui.attach.addEventListener('click', () => ui['file-input'].click());
ui['file-input'].addEventListener('change', async () => {
  const files = Array.from(ui['file-input'].files || []); ui['file-input'].value = '';
  for (const file of files) {
    if (!allowed('message')) { showError('Reconnect before uploading files.'); break; }
    const item = { name: file.name, uploading: true }; uploads.push(item); renderAttachments(); updateControls();
    try {
      item.metadata = await api('upload', { method: 'POST', headers: { 'Content-Type': file.type || 'application/octet-stream', 'X-Filename': encodeURIComponent(file.name) }, body: file });
      item.uploading = false;
    } catch (error) { uploads = uploads.filter((entry) => entry !== item); showError(new Error(`${file.name}: ${error.message}`)); }
    renderAttachments(); updateControls();
  }
});
ui.older.addEventListener('click', async () => {
  if (loadingHistory || historyBefore == null) return;
  loadingHistory = true; ui.older.disabled = true;
  const previousHeight = document.documentElement.scrollHeight;
  const previousY = window.scrollY;
  const epoch = historyEpoch;
  const anchor = historyAnchor;
  try {
    const page = await api(`messages?before=${encodeURIComponent(historyBefore)}`);
    if (epoch !== historyEpoch) return;
    const pageIds = new Set(page.entries.map((message) => message.id));
    const current = Array.from(retainedMessages.values()).filter((message) => !pageIds.has(message.id));
    const boundary = current.findIndex((message) => message.id === anchor);
    current.splice(boundary < 0 ? 0 : boundary, 0, ...page.entries);
    retainedMessages.clear();
    for (const message of current) retainedMessages.set(message.id, message);
    historyBefore = page.before ?? null;
    historyAnchor = page.entries[0]?.id ?? anchor;
    receive(state);
    window.scrollTo({ top: previousY + document.documentElement.scrollHeight - previousHeight, behavior: 'instant' });
  } catch (error) { showError(error); }
  finally { loadingHistory = false; ui.older.disabled = false; }
});
ui.latest.addEventListener('click', scrollLatest);
window.addEventListener('scroll', () => { ui.latest.hidden = nearBottom() || !state?.messages.length; }, { passive: true });
new ResizeObserver(([entry]) => { document.documentElement.style.setProperty('--dock-height', `${entry.target.getBoundingClientRect().height}px`); }).observe(document.querySelector('.composer-dock'));
let events;
let reconnect;
function connectEvents() {
  const stream = new EventSource('./api/events');
  events = stream;
  stream.addEventListener('state', (event) => {
    if (events !== stream) return;
    try { frameVersion++; connected = true; receive(JSON.parse(event.data)); }
    catch { showError('Unable to read the live session update. Reconnect to Crew.'); connection(false); }
  });
  stream.addEventListener('open', () => { if (events === stream) connection(true); });
  stream.addEventListener('error', () => {
    if (events !== stream) return;
    connection(false);
    if (stream.readyState === EventSource.CLOSED) {
      clearTimeout(reconnect);
      reconnect = setTimeout(connectEvents, 1500);
    }
  });
}
connectEvents();
const initialVersion = frameVersion;
api('session').then((initial) => { if (frameVersion === initialVersion) receive(initial); }).catch(showError);
api('models').then((catalog) => {
  if (!Array.isArray(catalog.models)) throw new Error('Crew returned an invalid model catalog.');
  modelCatalog = catalog.models;
  if (state) { renderModels(); updateControls(); }
}).catch(showError);
window.addEventListener('pagehide', () => { clearTimeout(reconnect); events.close(); events = null; });
window.addEventListener('pageshow', (event) => { if (event.persisted) { connection(false); connectEvents(); } });
