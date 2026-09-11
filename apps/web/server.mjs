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

export function loadConfig(env = process.env) {
  const profile = env.SCAFFOLD_COMET_RUNTIME_PROFILE_JSON ? JSON.parse(env.SCAFFOLD_COMET_RUNTIME_PROFILE_JSON) : {};
  const sessionId = env.CREW_WEB_SESSION_ID || profile.sessionId;
  requireValue(typeof sessionId === 'string' && sessionId.length > 0, 'Trusted session binding required');
  requireValue(!profile.sessionId || profile.sessionId === sessionId, 'Conflicting session binding');
  const port = Number(env.CREW_WEB_PORT || 4096);
  const ipcPort = Number(env.COMET_IPC_PORT);
  requireValue(Number.isInteger(port) && port > 0 && port <= 65535, 'Invalid HTTP port');
  requireValue(Number.isInteger(ipcPort) && ipcPort > 0 && ipcPort <= 65535, 'COMET_IPC_PORT required');
  requireValue(typeof env.CREW_WEB_AUTH_TOKEN === 'string' && env.CREW_WEB_AUTH_TOKEN.length >= 32, 'Trusted proxy token required');
  requireValue(env.COMET_DATA_DIR && env.SCAFFOLD_RUNTIME_DIR, 'COMET_DATA_DIR and SCAFFOLD_RUNTIME_DIR required');
  const sandboxId = profile.sandboxId || env.CREW_WEB_SANDBOX_ID;
  requireValue(typeof sandboxId === 'string' && /^[a-zA-Z0-9:_-]{1,256}$/.test(sandboxId), 'Trusted sandbox binding required');
  return { sessionId, sandboxId, host: env.CREW_WEB_HOST || '127.0.0.1', port, ipcPort,
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
      this.binding = { chatId: this.config.sessionId, roomProjection: authority.scope };
      await this.readContext();
      if (generation !== this.generation || this.stopped) return;
      this.rpc.watch('WatchSessions', {}, rows => {
        if (generation !== this.generation) return;
        this.snapshots.add('sessions');
        this.snapshots.delete('selection');
        this.live = rows.find(row => row.chatId === `${this.config.sessionId}::session::${this.config.sessionId}` && row.deviceId === authority.deviceId)
          || rows.find(row => row.chatId === this.config.sessionId && row.deviceId === authority.deviceId);
        this.rpc.call('ReadSessionSelection', { chatId: this.config.sessionId }).then(({ selection }) => {
          if (generation !== this.generation) return;
          this.snapshots.add('selection');
          this.selection = selection;
          this.emit();
        }).catch(() => { if (generation === this.generation) this.rpc.close(); });
        this.emit();
      });
      this.rpc.watch('WatchDocMessages', this.binding, frame => {
        if (generation !== this.generation) return;
        if (!this.snapshots.has('messages') && !Array.isArray(frame.reset)) throw new Error('Transcript reset required');
        this.snapshots.add('messages');
        this.messages = applyTranscriptFrame(this.messages, frame);
        this.before = frame.before ?? null;
        this.connection = 'connected';
        this.emit();
      });
      this.rpc.watch('WatchCollaboration', this.binding, snapshot => {
        if (generation !== this.generation) return;
        this.snapshots.add('collaboration');
        this.collaboration = snapshot;
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
      const models = await this.rpc.call('ListSessionModels', { chatId: this.config.sessionId }).catch(() => []);
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
    const authority = await this.rpc.call('ReadSessionAuthority', { chatId: this.config.sessionId });
    requireValue(generation === this.generation, 'Crew connection changed', 503);
    return this.acceptAuthority(authority, capability);
  }
  acceptAuthority(authority, capability) {
    requireValue(authority.scope?.sessionId === this.config.sessionId && (!this.config.sandboxId || authority.sandboxId === this.config.sandboxId), 'Crew session authority mismatch', 503);
    requireValue(authority.expiresAt > Date.now(), 'Crew authority expired', 503);
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
  async readContext() {
    const generation = this.generation;
    try {
      const context = await this.rpc.call('ReadSessionContext', { chatId: this.config.sessionId });
      requireValue(generation === this.generation, 'Crew connection changed', 503);
      this.acceptAuthority(context.authority, 'session.read');
      requireValue(typeof context.cwd === 'string' && context.cwd.startsWith('/') && context.config?.harness === 'omp', 'Invalid Crew session context', 503);
      this.context = context;
      this.snapshots.add('context');
      return context;
    } catch (error) {
      if (generation === this.generation) {
        this.clearProtected();
        this.rpc.close();
      }
      throw error;
    }
  }
  ready() {
    return this.connection === 'connected' && !!this.context
      && ['context', 'sessions', 'selection', 'messages', 'collaboration'].every(key => this.snapshots.has(key));
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
    this.selection = null;
    this.collaboration = null;
    this.messages = [];
    this.models = [];
    this.before = null;
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
    const record = this.collaboration?.sessions?.find(row => row.sessionId === this.config.sessionId);
    const status = this.live?.status || record?.status || 'idle';
    const active = this.ready() && this.currentAuthority?.expiresAt > Date.now();
    const caps = this.currentAuthority?.capabilities || [];
    return {
      sandboxId: this.config.sandboxId,
      session: { id: this.config.sessionId, title: context?.title || 'Crew', cwd: context?.cwd || '', branch: context?.branch || '', status,
        model: this.selection?.model || record?.model || context?.config?.model,
        reasoning: this.selection ? this.selection.reasoning : context?.config?.reasoning },
      messages: this.messages, models: this.models, history: { hasOlder: this.before != null, before: this.before ?? null },
      collaboration: this.collaboration, connection: this.connection,
      capabilities: { message: active && caps.includes(CAP_CHAT), input: active && caps.includes(CAP_CHAT), interrupt: active && caps.includes(CAP_CONTROL) },
    };
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
          if (client.writableLength > 2 * 1024 * 1024 || !client.write(frame)) {
            client.destroy();
            this.clients.delete(client);
          }
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
    const allowed = kind === 'message' ? ['requestId', 'text', 'model', 'reasoning', 'attachments'] : kind === 'input' ? ['requestId', 'inputRequestId', 'answers'] : ['requestId'];
    object(input, allowed);
    const requestId = identifier(input.requestId);
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
    await this.readContext();
    const authority = await this.readAuthority();
    requireValue(authority.capabilities.includes(kind === 'interrupt' ? CAP_CONTROL : CAP_CHAT), 'Session capability unavailable', 403);
    let action;
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
        action = { action: 'steer', prompt, message_id: `crew-web:${requestId}` };
      } else {
        action = { action: 'start', message_id: `crew-web:${requestId}`, request: {
          prompt, model, reasoning, modelOptions: this.context.config?.modelOptions || {}, cwd: this.context.cwd,
          sandbox: this.context.config?.sandbox || 'workspace-write', autoApprove: false,
          resume: null,
          attachments: images,
        } };
      }
    } else if (kind === 'interrupt') {
      action = { action: 'stop' };
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
      action = { action: 'respondInput', request_id: input.inputRequestId, answers: input.answers };
    }
    const command = { kind: 'control', sessionId: this.config.sessionId, ownerDeviceId: authority.deviceId,
      actorDeviceId: `crew-web:${this.config.sessionId}`, actorSubject: authority.principalSubject,
      grantId: authority.grantId, source: 'scaffold', action };
    record = { requestHash, commandId: `crew-web:${requestId}`, command };
    await atomicJson(recordPath, record);
    return this.queueRecord(recordPath, record);
  }
  async queueRecord(path, record) {
    const receipt = await this.rpc.call('QueueCommand', { chatId: this.config.sessionId, commandId: record.commandId, command: record.command });
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
    const { command } = await this.rpc.call('ReadSessionCommand', { chatId: this.config.sessionId, commandId: record.commandId });
    await this.readAuthority();
    return command || { commandId: record.commandId, status: 'pending', resolution: null };
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
      if (req.method === 'GET' && path === '/health') return respond(res, 200, { service: 'crew-web', sessionId: config.sessionId, sandboxId: config.sandboxId });
      if (req.method === 'GET' && path === '/api/session') return respond(res, 200, viewport.state());
      if (req.method === 'GET' && path === '/api/models') {
        await viewport.readAuthority();
        if (!viewport.models.length) viewport.models = await viewport.rpc.call('ListSessionModels', { chatId: config.sessionId });
        await viewport.readAuthority();
        return respond(res, 200, { models: viewport.models });
      }
      if (req.method === 'GET' && path === '/api/events') {
        requireValue(viewport.clients.size < 16, 'Too many event streams', 429);
        res.writeHead(200, { 'Content-Type': 'text/event-stream', Connection: 'keep-alive', 'X-Accel-Buffering': 'no' });
        res.write(`event: state\ndata: ${JSON.stringify(viewport.state())}\n\n`);
        viewport.clients.add(res);
        const heartbeat = setInterval(() => viewport.emit(), 15000);
        res.on('close', () => { clearInterval(heartbeat); viewport.clients.delete(res); });
        return;
      }
      if (req.method === 'GET' && path === '/api/messages') {
        const before = Number(url.searchParams.get('before'));
        requireValue(url.searchParams.has('before') && Number.isSafeInteger(before) && before >= 0, 'Invalid history cursor');
        const page = await viewport.rpc.call('ReadDocMessages', { ...viewport.binding, before });
        await viewport.readAuthority();
        return respond(res, 200, page);
      }
      if (req.method === 'GET' && path.startsWith('/api/tool/')) {
        const toolId = decodeURIComponent(path.slice('/api/tool/'.length));
        requireValue(toolId.length > 0 && toolId.length <= 512, 'Invalid tool identifier');
        let detail = await viewport.rpc.call('ToolCallDetail', { chatId: `${config.sessionId}::session::${config.sessionId}`, toolId });
        if (!detail.found) detail = await viewport.rpc.call('ToolCallDetail', { chatId: config.sessionId, toolId });
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
