import http from 'node:http';
import { readFile, writeFile, mkdir, stat, rename } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash, randomUUID, timingSafeEqual } from 'node:crypto';
import { CrewRpc, applyTranscriptFrame } from './rpc.mjs';

const publicDir = join(dirname(fileURLToPath(import.meta.url)), 'public');
const JSON_LIMIT = 256 * 1024;
const UPLOAD_LIMIT = 16 * 1024 * 1024;
const CAP_CHAT = 'session.chat';
const CAP_CONTROL = 'session.control';

export class HttpError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}
function requireValue(ok, message, status = 400) { if (!ok) throw new HttpError(status, message); }
function object(value, keys) {
  requireValue(value && typeof value === 'object' && !Array.isArray(value), 'Expected JSON object');
  requireValue(Object.keys(value).every(key => keys.includes(key)), 'Unknown request field');
  return value;
}
function identifier(value) {
  requireValue(typeof value === 'string' && /^[a-zA-Z0-9:_-]{1,160}$/.test(value), 'Invalid request identifier');
  return value;
}
function digest(value) { return createHash('sha256').update(value).digest('hex'); }

export function browserEntry(entry) {
  const parts = [];
  let size = 0;
  for (let index = entry.parts.length - 1; index >= 0; index--) {
    let part = entry.parts[index];
    if (part.kind === 'reasoning') part = { id: part.id, kind: 'thinking', started: true, completed: entry.status !== 'streaming' };
    if (part.kind === 'text') {
      const bytes = Buffer.from(part.text);
      if (bytes.length > 32768) {
        let start = bytes.length - 32768;
        while ((bytes[start] & 0xc0) === 0x80) start++;
        part = { ...part, kind: 'textWindow', text: bytes.subarray(start).toString('utf8'), omitted_prefix_bytes: start };
      }
    }
    let bytes = Buffer.byteLength(JSON.stringify(part));
    if (bytes > 128 * 1024) {
      part = { id: part.id, kind: 'error', message: 'This part exceeds the browser display limit. Open native Crew to inspect it.' };
      bytes = Buffer.byteLength(JSON.stringify(part));
    }
    if (size + bytes > 256 * 1024 || parts.length >= 100) {
      parts.unshift({ id: `${entry.id}:omitted`, kind: 'error', message: 'Earlier parts omitted from this browser window. Open native Crew for the full entry.' });
      break;
    }
    parts.unshift(part);
    size += bytes;
  }
  return { id: entry.id, role: entry.role, status: entry.status, createdAt: entry.createdAt, continuationOf: entry.continuationOf, durationMs: entry.durationMs, parts };
}

export function loadConfig(env = process.env) {
  requireValue(env.SCAFFOLD_CREW_SYNC_GENERATION === 'chat2', 'Trusted chat2 runtime required');
  const profile = JSON.parse(env.SCAFFOLD_COMET_RUNTIME_PROFILE_JSON || '{}');
  const sessionId = identifier(profile.sessionId);
  requireValue(!env.CREW_WEB_SESSION_ID || env.CREW_WEB_SESSION_ID === sessionId, 'Conflicting session binding');
  for (const value of [profile.projectId, profile.deploymentId, profile.sandboxId, profile.targetDeviceId]) identifier(value);
  requireValue(typeof profile.actor?.sub === 'string' && profile.actor.sub.length > 0, 'Trusted owner binding required');
  requireValue(Number.isSafeInteger(profile.lifecycleEpoch) && profile.lifecycleEpoch > 0, 'Trusted lifecycle epoch required');
  const port = Number(env.CREW_WEB_PORT || 4096);
  const ipcPort = Number(env.COMET_IPC_PORT);
  requireValue(Number.isInteger(port) && port > 0 && port <= 65535, 'Invalid HTTP port');
  requireValue(ipcPort === 27954, 'Canary COMET_IPC_PORT must be 27954');
  requireValue(typeof env.CREW_WEB_AUTH_TOKEN === 'string' && env.CREW_WEB_AUTH_TOKEN.length >= 32, 'Trusted proxy token required');
  requireValue(env.COMET_DATA_DIR?.startsWith('/') && env.COMET_DATA_DIR.endsWith('/.scaffold/crew-upstream-canary/data') && env.SCAFFOLD_RUNTIME_DIR?.startsWith('/'), 'Canonical canary data and runtime directories required');
  return { sessionId, sandboxId: profile.sandboxId, projectId: profile.projectId, deploymentId: profile.deploymentId,
    deviceId: profile.targetDeviceId, ownerSubject: profile.actor.sub, lifecycleEpoch: profile.lifecycleEpoch,
    host: env.CREW_WEB_HOST || '127.0.0.1', port, ipcPort,
    token: env.CREW_WEB_AUTH_TOKEN, storage: join(env.COMET_DATA_DIR, 'crew-web', digest(sessionId)), runtimeDir: env.SCAFFOLD_RUNTIME_DIR };
}

export function authorize(req, config) {
  const expected = Buffer.from(`Basic ${Buffer.from(`opencode:${config.token}`).toString('base64')}`);
  const actual = Buffer.from(req.headers.authorization || '');
  requireValue(actual.length === expected.length && timingSafeEqual(actual, expected), 'Unauthorized', 401);
  // Only the trusted attach proxy may set this marker, after stripping the
  // incoming value and validating Origin against its public host/protocol.
  if (!['GET', 'HEAD'].includes(req.method)) {
    requireValue(req.headers['x-crew-web-origin-verified'] === 'true', 'Verified same-origin mutation required', 403);
  }
}

async function body(req, limit) {
  if (Number(req.headers['content-length']) > limit) throw new HttpError(413, 'Request too large');
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) throw new HttpError(413, 'Request too large');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks, size);
}
async function jsonBody(req) {
  requireValue(req.headers['content-type']?.split(';')[0] === 'application/json', 'JSON content type required', 415);
  try { return JSON.parse((await body(req, JSON_LIMIT)).toString()); }
  catch (error) { if (error instanceof HttpError) throw error; throw new HttpError(400, 'Invalid JSON'); }
}
async function readJson(path) { return JSON.parse(await readFile(path, 'utf8')); }
async function atomicJson(path, value) {
  const temp = `${path}.${randomUUID()}.tmp`;
  await writeFile(temp, JSON.stringify(value), { mode: 0o600, flag: 'wx' });
  await rename(temp, path);
}

export class Viewport {
  constructor(config, rpc = new CrewRpc(config.ipcPort)) {
    this.config = config;
    this.rpc = rpc;
    this.clients = new Set();
    this.blockedClients = new WeakSet();
    this.messages = [];
    this.models = [];
    this.connection = 'disconnected';
    this.serial = Promise.resolve();
    this.stopped = false;
    this.uploadsInFlight = 0;
    this.generation = 0;
    this.snapshots = new Set();
    this.rpc.on('disconnect', () => {
      this.generation++;
      clearInterval(this.authorityTimer);
      this.clearProtected();
      this.scheduleReconnect();
    });
  }
  scheduleReconnect() {
    if (!this.stopped && !this.reconnect) this.reconnect = setTimeout(() => {
      this.reconnect = null;
      this.connect();
    }, 1500);
  }
  async connect() {
    const generation = ++this.generation;
    try {
      await this.rpc.connect();
      const authority = await this.readAuthority();
      if (generation !== this.generation || this.stopped) return;
      this.binding = { chatId: this.config.sessionId };
      this.rpc.watch('WatchChats', {}, rows => {
        if (generation !== this.generation) return;
        try {
          this.context = rows.find(row => row.id === this.config.sessionId);
          this.acceptContext();
        } catch (error) {
          this.clearProtected();
          this.rpc.close();
          throw error;
        }
        this.snapshots.add('context');
        this.emit();
      });
      this.rpc.watch('WatchSessions', {}, rows => {
        if (generation !== this.generation) return;
        this.snapshots.add('sessions');
        this.live = rows.find(row => row.chatId === this.config.sessionId && row.deviceId === authority.deviceId);
        this.emit();
      });
      this.rpc.watch('WatchDocMessages', this.binding, frame => {
        if (generation !== this.generation) return;
        if (!this.snapshots.has('messages') && !Array.isArray(frame.reset)) throw new Error('Transcript reset required');
        this.messages = applyTranscriptFrame(this.messages, frame);
        // ponytail: mirror the native stream; a native paged RPC is needed above 64 MiB.
        requireValue(Buffer.byteLength(JSON.stringify(this.messages)) <= 64 * 1024 * 1024, 'Transcript mirror limit exceeded', 503);
        this.snapshots.add('messages');
        this.connection = 'connected';
        this.emit();
      });
      let refreshing = false;
      this.authorityTimer = setInterval(async () => {
        if (refreshing || generation !== this.generation) return;
        refreshing = true;
        try { await this.readAuthority(); this.emit(); }
        catch { if (generation === this.generation) this.rpc.close(); }
        finally { refreshing = false; }
      }, 15000);
      const models = await this.rpc.call('ListModels', { harness: 'omp' });
      if (generation !== this.generation) return;
      this.models = models;
      this.emit();
    } catch {
      if (generation !== this.generation) return;
      this.connection = 'disconnected';
      this.rpc.close();
      this.emit();
      this.scheduleReconnect();
    }
  }
  async authority(capability) {
    const generation = this.generation;
    const authority = await this.rpc.call('ScaffoldHostAuthority', {});
    requireValue(generation === this.generation, 'Crew connection changed', 503);
    return this.acceptAuthority(authority, capability);
  }
  acceptAuthority(authority, capability) {
    requireValue(authority.scope?.sessionId === this.config.sessionId && authority.sandboxId === this.config.sandboxId
      && authority.scope.projectId === this.config.projectId && authority.scope.deploymentId === this.config.deploymentId
      && authority.deviceId === this.config.deviceId && authority.principalSubject === this.config.ownerSubject
      && authority.lifecycleEpoch === this.config.lifecycleEpoch && typeof authority.grantId === 'string' && authority.grantId.length > 0,
    'Crew session authority mismatch', 503);
    requireValue(Number.isSafeInteger(authority.expiresAt) && authority.expiresAt > Date.now(), 'Crew authority expired', 503);
    if (this.currentAuthority) {
      requireValue(authority.deviceId === this.currentAuthority.deviceId
        && authority.principalSubject === this.currentAuthority.principalSubject
        && authority.lifecycleEpoch === this.currentAuthority.lifecycleEpoch
        && authority.scope.projectId === this.currentAuthority.scope.projectId
        && authority.scope.deploymentId === this.currentAuthority.scope.deploymentId,
      'Crew authority binding changed', 503);
    }
    if (capability) requireValue(authority.capabilities?.includes(capability), 'Session capability unavailable', 403);
    this.currentAuthority = authority;
    return authority;
  }
  acceptContext() {
    const context = this.context;
    requireValue(context?.id === this.config.sessionId && context.deviceId === this.config.deviceId
      && typeof context.cwd === 'string' && context.cwd.startsWith('/') && context.config?.harness === 'omp'
      && context.roomGen === 2, 'Invalid Crew session context', 503);
    if (context.crew) {
      requireValue(context.crew.sessionId === this.config.sessionId
        && context.crew.scope?.projectId === this.config.projectId && context.crew.scope?.deploymentId === this.config.deploymentId
        && context.crew.scope?.sessionId === this.config.sessionId, 'Crew chat identity mismatch', 503);
    }
    return context;
  }
  async readContext() {
    await this.readAuthority();
    return this.acceptContext();
  }
  ready() {
    return this.connection === 'connected' && !!this.context
      && ['context', 'sessions', 'messages'].every(key => this.snapshots.has(key));
  }
  canRead() {
    return !!this.currentAuthority && this.currentAuthority.expiresAt > Date.now()
      && this.currentAuthority.capabilities.includes('session.read');
  }
  clearProtected() {
    this.snapshots.clear();
    this.currentAuthority = null;
    this.context = null;
    this.live = null;
    this.messages = [];
    this.models = [];
    this.connection = 'disconnected';
    const frame = `event: state\ndata: ${JSON.stringify(this.state())}\n\n`;
    for (const client of this.clients) client.end(frame);
    this.clients.clear();
  }
  async readAuthority() {
    const generation = this.generation;
    try { return await this.authority('session.read'); }
    catch (error) {
      if (generation === this.generation) {
        this.clearProtected();
        this.rpc.close();
      }
      throw error;
    }
  }
  state() {
    if (!this.canRead()) return {
      sandboxId: this.config.sandboxId,
      session: { id: this.config.sessionId, title: 'Crew', cwd: '', branch: '', status: 'idle' },
      messages: [], models: [], history: { hasOlder: false, before: null },
      transcriptReset: true,
      connection: 'disconnected', capabilities: { message: false, input: false, interrupt: false },
    };
    const context = this.context;
    const status = this.live?.status || 'idle';
    const page = this.messagePage();
    const active = this.ready() && this.currentAuthority?.expiresAt > Date.now();
    const caps = this.currentAuthority?.capabilities || [];
    return {
      sandboxId: this.config.sandboxId,
      session: { id: this.config.sessionId, title: context?.title || 'Crew', cwd: context?.cwd || '', branch: context?.branch || '', status,
        turnId: ['working', 'awaitingInput'].includes(status) ? this.messages.at(-1)?.id ?? null : null,
        model: context?.config?.model, reasoning: context?.config?.reasoning },
      messages: page.entries, models: this.models, history: { hasOlder: page.before != null, before: page.before },
      connection: this.connection,
      capabilities: { message: active && caps.includes(CAP_CHAT), input: active && caps.includes(CAP_CHAT), interrupt: active && caps.includes(CAP_CONTROL) },
    };
  }
  messagePage(before = this.messages.length) {
    requireValue(Number.isSafeInteger(before) && before >= 0 && before <= this.messages.length, 'Invalid history cursor', 409);
    const entries = [];
    let size = 0;
    let cursor = before;
    while (cursor > 0 && entries.length < 100) {
      const entry = browserEntry(this.messages[cursor - 1]);
      const bytes = Buffer.byteLength(JSON.stringify(entry));
      if (entries.length && size + bytes > 512 * 1024) break;
      entries.unshift(entry);
      size += bytes;
      cursor--;
    }
    return { entries, before: cursor || null };
  }
  sendState(client, frame) {
    if (client.writableLength + Buffer.byteLength(frame) > 2 * 1024 * 1024) {
      this.clients.delete(client);
      client.destroy();
      return;
    }
    if (this.blockedClients.has(client)) return;
    if (!client.write(frame)) {
      this.blockedClients.add(client);
      client.once('drain', () => {
        this.blockedClients.delete(client);
        if (this.clients.has(client)) void this.emit();
      });
    }
  }
  async emit() {
    if (!this.clients.size) return;
    this.emitPending = true;
    if (this.emitting) return;
    this.emitting = true;
    try {
      // Coalesce updates arriving while the fresh capability check is in flight;
      // never deliver protected cached state using only the attach credential.
      while (this.emitPending && this.clients.size) {
        this.emitPending = false;
        await this.readAuthority();
        const frame = `event: state\ndata: ${JSON.stringify(this.state())}\n\n`;
        for (const client of this.clients) {
          this.sendState(client, frame);
        }
      }
    } catch {
      // readAuthority has already erased the cache and ended subscribers with
      // a redacted disconnected state. Reconnect requires fresh snapshots.
    } finally { this.emitting = false; }
  }
  async approvedModel(model) {
    const path = join(this.config.runtimeDir, 'omp-inference', 'profile.json');
    requireValue((await stat(path)).size <= 4096, 'Invalid inference profile', 503);
    const profile = await readJson(path);
    const routed = model?.replace(/^openai-codex\//, 'scaffold-openai/').replace(/^anthropic\//, 'scaffold-anthropic/');
    requireValue(profile.profile === 'scaffold-host' && profile.model === routed, 'Model route must be authorized before sending', 409);
  }
  async enqueue(kind, input) {
    // Serialize admission (not generation) so retries and mode selection share a
    // single immutable request record, including across process restarts.
    const task = this.serial.then(() => this.admit(kind, input));
    this.serial = task.catch(() => {});
    return task;
  }
  async admit(kind, input) {
    const allowed = kind === 'message' ? ['requestId', 'text', 'model', 'reasoning', 'attachments'] : kind === 'input' ? ['requestId', 'inputRequestId', 'answers'] : ['requestId', 'turnId'];
    object(input, allowed);
    const requestId = identifier(input.requestId);
    await this.readContext();
    await this.authority(kind === 'interrupt' ? CAP_CONTROL : CAP_CHAT);
    const requestHash = digest(JSON.stringify({ kind, input }));
    const recordPath = join(this.config.storage, `command-${digest(requestId)}.json`);
    let record;
    try { record = await readJson(recordPath); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    if (record) {
      requireValue(record.requestHash === requestHash, 'Request identifier already used for different content', 409);
      if (record.receipt) return record.receipt;
      return this.queueRecord(recordPath, record);
    }
    requireValue(this.ready(), 'Assigned Crew session unavailable', 503);
    const generation = this.generation;
    let command;
    const commandId = `crew-web:${this.config.lifecycleEpoch}:${requestId}`;
    if (kind === 'message') {
      requireValue(typeof input.text === 'string' && input.text.length <= 128 * 1024, 'Invalid message text');
      requireValue(Array.isArray(input.attachments) && input.attachments.length <= 16, 'Invalid attachments');
      const attachments = [];
      for (const reference of input.attachments) {
        object(reference, ['id', 'name', 'size', 'type']);
        const attachment = await readJson(join(this.config.storage, `upload-${digest(identifier(reference.id))}.json`)).catch(() => null);
        requireValue(attachment, 'Unknown attachment', 400);
        attachments.push(attachment);
      }
      requireValue(input.text.trim() || attachments.length, 'Empty message');
      const images = attachments.filter(file => file.type.startsWith('image/')).map(file => file.path);
      const files = attachments.filter(file => !file.type.startsWith('image/')).map(file => file.path);
      let prompt = input.text.trim() || (files.length ? 'See the attached file(s).' : 'See the attached image(s).');
      if (images.length) prompt += '\n\nAttached images (local files — open them to view):' + images.map(path => `\n- ${path}`).join('');
      if (files.length) prompt += '\n\nAttached files (local files — open them to inspect):' + files.map(path => `\n- ${path}`).join('');
      const state = this.state().session;
      const model = input.model || state.model;
      requireValue(typeof model === 'string' && model.length < 256, 'Model required');
      const reasoning = Object.hasOwn(input, 'reasoning') ? input.reasoning : state.reasoning ?? null;
      requireValue(reasoning === null || ['minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', 'ultracode', 'ultrathink'].includes(reasoning), 'Invalid reasoning');
      await this.approvedModel(model);
      requireValue(generation === this.generation && this.ready(), 'Crew session state changed; retry', 503);
      const currentState = this.state().session;
      if (['working', 'awaitingInput'].includes(currentState.status)) {
        requireValue(model === currentState.model && reasoning === (currentState.reasoning ?? null), 'Stop the current turn before changing model or reasoning', 409);
        command = { kind: 'steer', prompt, messageId: commandId };
      } else {
        command = { kind: 'run', messageId: commandId, request: {
          prompt, harness: 'omp', model, reasoning, modelOptions: this.context.config?.modelOptions || {}, cwd: this.context.cwd,
          sandbox: this.context.config?.sandbox || 'workspace-write', autoApprove: false,
          resume: null,
          attachments: images,
        } };
      }
    } else if (kind === 'interrupt') {
      requireValue(typeof input.turnId === 'string' && input.turnId.length > 0 && input.turnId.length <= 2048, 'Expected turn required');
      requireValue(input.turnId === this.state().session.turnId && ['working', 'awaitingInput'].includes(this.state().session.status), 'Stop target is no longer active', 409);
      command = { kind: 'interrupt' };
    } else {
      requireValue(typeof input.inputRequestId === 'string' && input.inputRequestId.length > 0 && input.inputRequestId.length <= 512, 'Invalid input request');
      const pending = this.messages.flatMap(entry => entry.parts).find(part => part.kind === 'input' && !part.resolved && part.requestId === input.inputRequestId);
      requireValue(pending, 'Input request is no longer pending', 409);
      requireValue(Array.isArray(input.answers) && input.answers.length === pending.questions.length, 'Every question needs an answer');
      const seen = new Set();
      for (const answer of input.answers) {
        object(answer, ['questionId', 'labels']);
        const question = pending.questions.find(row => row.id === answer.questionId);
        requireValue(question && !seen.has(answer.questionId), 'Unknown or duplicate question');
        seen.add(answer.questionId);
        requireValue(Array.isArray(answer.labels) && answer.labels.length > 0 && answer.labels.length <= 32 && answer.labels.every(label => typeof label === 'string' && label.length <= 8192), 'Invalid input answer');
        requireValue(question.multiSelect || answer.labels.length === 1, 'Question requires a single answer');
      }
      command = { kind: 'respondInput', requestId: input.inputRequestId, answers: input.answers };
    }
    record = { requestHash, commandId, command, lifecycleEpoch: this.config.lifecycleEpoch,
      expectedTurnId: kind === 'interrupt' ? input.turnId : undefined };
    await atomicJson(recordPath, record);
    return this.queueRecord(recordPath, record);
  }
  async queueRecord(path, record) {
    await this.readContext();
    await this.authority(record.command.kind === 'interrupt' ? CAP_CONTROL : CAP_CHAT);
    requireValue(record.lifecycleEpoch === this.config.lifecycleEpoch, 'Command belongs to an earlier lifecycle; use a new request identifier', 409);
    if (record.command.kind === 'run') await this.approvedModel(record.command.request.model);
    const receipt = await this.rpc.call('QueueCommand', { chatId: this.config.sessionId, commandId: record.commandId,
      command: record.command, ...(record.expectedTurnId ? { expectedTurnId: record.expectedTurnId } : {}) });
    await atomicJson(path, { ...record, receipt });
    return receipt;
  }
  async commandOutcome(requestId) {
    await this.readAuthority();
    identifier(requestId);
    const record = await readJson(join(this.config.storage, `command-${digest(requestId)}.json`)).catch(error => {
      if (error.code === 'ENOENT') throw new HttpError(404, 'Unknown viewport request');
      throw error;
    });
    const { command } = await this.rpc.call('ReadCommand', { chatId: this.config.sessionId, commandId: record.commandId });
    await this.readAuthority();
    return { commandId: record.commandId, status: command?.status || 'pending', resolution: command?.resolution ?? null };
  }
  async toolDetail(toolId) {
    const part = this.messages.flatMap(entry => entry.parts).find(part => part.kind === 'tool' && part.id === toolId);
    requireValue(part, 'Unknown tool in assigned session', 404);
    let output = part.output ?? '';
    for (const [ref, suffix] of [[part.outputRef, ''], [part.diffRef, '.diff']]) {
      if (!ref) continue;
      requireValue(ref === `${this.config.sessionId}/${toolId}${suffix}`, 'Tool blob session mismatch', 503);
      const { text } = await this.rpc.call('FetchToolBlob', { blobRef: ref });
      requireValue(typeof text === 'string' && Buffer.byteLength(text) <= 512 * 1024, 'Tool detail exceeds browser limit', 413);
      output = suffix ? `${output}\n\nDiff:\n${text}` : text;
    }
    const detail = { found: true, resolved: part.resolved, isError: part.isError, input: JSON.stringify(part.call, null, 2), output };
    requireValue(Buffer.byteLength(JSON.stringify(detail)) <= 1024 * 1024, 'Tool detail exceeds browser limit', 413);
    return detail;
  }
  async upload(req) {
    requireValue(this.ready(), 'Assigned Crew session unavailable', 503);
    const generation = this.generation;
    await this.readContext();
    const authority = await this.readAuthority();
    requireValue(authority.capabilities.includes(CAP_CHAT), 'Session capability unavailable', 403);
    let name;
    try { name = decodeURIComponent(req.headers['x-filename'] || ''); } catch { throw new HttpError(400, 'Invalid file name'); }
    requireValue(name.length > 0 && name.length <= 200 && !/[\x00-\x1f/\\]/.test(name) && name !== '.' && name !== '..', 'Invalid file name');
    const bytes = await body(req, UPLOAD_LIMIT);
    requireValue(bytes.length > 0, 'Empty attachment');
    const id = randomUUID();
    const chunkSize = 45000;
    for (let offset = 0, seq = 0; offset < bytes.length; offset += chunkSize, seq++) {
      await this.authority(CAP_CHAT);
      requireValue(generation === this.generation && this.ready(), 'Crew session state changed; retry', 503);
      await this.rpc.call('UploadChunk', { uploadId: id, seq, data: bytes.subarray(offset, offset + chunkSize).toString('base64') });
    }
    await this.authority(CAP_CHAT);
    requireValue(generation === this.generation && this.ready(), 'Crew session state changed; retry', 503);
    const { path } = await this.rpc.call('UploadCommit', { uploadId: id, fileName: name });
    const type = /^image\/(png|jpeg|gif|webp)$/.test(req.headers['content-type'] || '') ? req.headers['content-type'] : 'application/octet-stream';
    const metadata = { id, name, size: bytes.length, type };
    await atomicJson(join(this.config.storage, `upload-${digest(id)}.json`), { ...metadata, path });
    return metadata;
  }
  stop() {
    this.stopped = true;
    this.generation++;
    clearInterval(this.authorityTimer);
    clearTimeout(this.reconnect);
    this.clearProtected();
    this.rpc.close();
  }
}

function respond(res, status, data) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(data));
}
export async function createServer(config, viewport = new Viewport(config)) {
  await mkdir(config.storage, { recursive: true, mode: 0o700 });
  const server = http.createServer(async (req, res) => {
    res.setHeader('X-Crew-Web-Service', 'crew-web');
    res.setHeader('X-Crew-Web-Sandbox-Id', config.sandboxId);
    res.setHeader('Cache-Control', 'no-store');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Referrer-Policy', 'no-referrer');
    res.setHeader('Content-Security-Policy', "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' blob: data:; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'");
    try {
      authorize(req, config);
      const url = new URL(req.url, 'http://crew.invalid');
      const path = url.pathname;
      if (req.method === 'GET' && (['/api/session', '/api/events', '/api/messages'].includes(path) || path.startsWith('/api/tool/'))) await viewport.readAuthority();
      if (req.method === 'GET' && path === '/health') {
        await viewport.readContext();
        requireValue(viewport.ready(), 'Assigned Crew session unavailable', 503);
        return respond(res, 200, { service: 'crew-web', crewSyncGeneration: 'chat2', sessionId: config.sessionId, sandboxId: config.sandboxId });
      }
      if (req.method === 'GET' && path === '/api/session') return respond(res, 200, viewport.state());
      if (req.method === 'GET' && path === '/api/native-open') {
        const authority = await viewport.readAuthority();
        const scope = authority.scope;
        const segments = [scope?.projectId, scope?.deploymentId, scope?.sessionId, config.sandboxId];
        requireValue(segments.every(value => typeof value === 'string' && /^[A-Za-z0-9._-]{1,256}$/.test(value))
          && scope.sessionId === config.sessionId && authority.sandboxId === config.sandboxId,
        'Crew native session binding unavailable', 503);
        return respond(res, 200, { url: `crew-upstream-canary://scaffold/${segments.join('/')}` });
      }
      if (req.method === 'GET' && path === '/api/models') {
        await viewport.readAuthority();
        if (!viewport.models.length) viewport.models = await viewport.rpc.call('ListModels', { harness: 'omp' });
        await viewport.readAuthority();
        return respond(res, 200, { models: viewport.models });
      }
      if (req.method === 'GET' && path === '/api/events') {
        requireValue(viewport.clients.size < 16, 'Too many event streams', 429);
        res.writeHead(200, { 'Content-Type': 'text/event-stream', Connection: 'keep-alive', 'X-Accel-Buffering': 'no' });
        viewport.clients.add(res);
        viewport.sendState(res, `event: state\ndata: ${JSON.stringify(viewport.state())}\n\n`);
        const heartbeat = setInterval(() => viewport.emit(), 15000);
        res.on('close', () => { clearInterval(heartbeat); viewport.clients.delete(res); });
        return;
      }
      if (req.method === 'GET' && path === '/api/messages') {
        const before = Number(url.searchParams.get('before'));
        requireValue(url.searchParams.has('before') && Number.isSafeInteger(before) && before >= 0, 'Invalid history cursor');
        const page = viewport.messagePage(before);
        await viewport.readAuthority();
        return respond(res, 200, page);
      }
      if (req.method === 'GET' && path.startsWith('/api/tool/')) {
        const toolId = decodeURIComponent(path.slice('/api/tool/'.length));
        requireValue(toolId.length > 0 && toolId.length <= 512, 'Invalid tool identifier');
        const detail = await viewport.toolDetail(toolId);
        await viewport.readAuthority();
        return respond(res, 200, detail);
      }
      if (req.method === 'GET' && path.startsWith('/api/command/')) {
        return respond(res, 200, await viewport.commandOutcome(decodeURIComponent(path.slice('/api/command/'.length))));
      }
      if (req.method === 'POST' && path === '/api/upload') {
        requireValue(viewport.uploadsInFlight < 4, 'Too many concurrent uploads', 429);
        viewport.uploadsInFlight++;
        try { return respond(res, 201, await viewport.upload(req)); }
        finally { viewport.uploadsInFlight--; }
      }
      if (req.method === 'POST' && ['/api/message', '/api/interrupt', '/api/input'].includes(path)) {
        return respond(res, 202, await viewport.enqueue(path.slice('/api/'.length), await jsonBody(req)));
      }
      if (req.method === 'GET' && ['/', '/index.html', '/app.js', '/style.css'].includes(path)) {
        const file = path === '/' ? 'index.html' : path.slice(1);
        const bytes = await readFile(join(publicDir, file));
        res.writeHead(200, { 'Content-Type': file.endsWith('.html') ? 'text/html; charset=utf-8' : file.endsWith('.js') ? 'text/javascript; charset=utf-8' : 'text/css; charset=utf-8' });
        return res.end(bytes);
      }
      throw new HttpError(404, 'Not found');
    } catch (error) {
      if (res.headersSent) return res.destroy();
      respond(res, error.status || 503, { error: error instanceof HttpError ? error.message : 'Crew engine request failed' });
    }
  });
  server.requestTimeout = 30000;
  server.headersTimeout = 15000;
  server.maxHeadersCount = 40;
  server.maxConnections = 64;
  server.on('close', () => viewport.stop());
  return { server, viewport };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const config = loadConfig();
  const { server, viewport } = await createServer(config);
  server.listen(config.port, config.host, () => {
    console.log(`Crew web listening on ${config.host}:${config.port}`);
    viewport.connect();
  });
  const shutdown = () => { viewport.stop(); server.close(); server.closeAllConnections(); };
  process.once('SIGTERM', shutdown);
  process.once('SIGINT', shutdown);
}
