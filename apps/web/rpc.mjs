import { EventEmitter } from 'node:events';

export class CrewRpc extends EventEmitter {
  constructor(port) {
    super();
    this.url = `ws://127.0.0.1:${port}`;
    this.pending = new Map();
    this.nextId = 0;
  }

  async connect() {
    if (this.socket) throw new Error('RPC already connected');
    const socket = new WebSocket(this.url);
    this.socket = socket;
    await new Promise((resolve, reject) => {
      const disconnect = error => {
        if (this.socket !== socket) return;
        clearTimeout(timer);
        this.socket = null;
        for (const pending of this.pending.values()) {
          clearTimeout(pending.timer);
          pending.reject(error);
        }
        this.pending.clear();
        reject(error);
        this.emit('disconnect');
      };
      const timer = setTimeout(() => {
        disconnect(new Error('Crew connection timed out'));
        socket.close();
      }, 10000);
      socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
      socket.addEventListener('error', () => {
        disconnect(new Error('Crew engine unavailable'));
        socket.close();
      }, { once: true });
      socket.addEventListener('close', () => disconnect(new Error('Crew engine disconnected')), { once: true });
      socket.addEventListener('message', ({ data }) => {
        if (this.socket !== socket) return;
        try {
          if (typeof data !== 'string' || Buffer.byteLength(data) > 32 * 1024 * 1024) throw new Error('Invalid RPC frame');
          const frame = JSON.parse(data);
          const pending = this.pending.get(frame.id);
          if (!pending) return;
          if ('err' in frame) {
            clearTimeout(pending.timer);
            this.pending.delete(frame.id);
            pending.reject(new Error(typeof frame.err === 'string' ? frame.err : JSON.stringify(frame.err)));
          } else if ('ok' in frame) {
            clearTimeout(pending.timer);
            this.pending.delete(frame.id);
            pending.resolve(frame.ok);
          } else if ('item' in frame) {
            clearTimeout(pending.timer);
            pending.item(frame.item);
          } else if (frame.done) {
            clearTimeout(pending.timer);
            this.pending.delete(frame.id);
            pending.reject(new Error('Crew subscription ended'));
          }
        } catch {
          socket.close();
        }
      });
    });
  }

  request(method, params, item) {
    if (this.socket?.readyState !== WebSocket.OPEN) return Promise.reject(new Error('Crew engine disconnected'));
    const id = ++this.nextId;
    const startedAt = Date.now();
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        this.socket?.send(JSON.stringify({ id, cancel: true }));
        const elapsedMs = Date.now() - startedAt;
        console.error('Crew RPC request timed out', { method, id, elapsedMs });
        reject(new Error(`Crew request timed out: method=${method} id=${id} elapsed_ms=${elapsedMs}`));
      }, 30000);
      this.pending.set(id, { resolve, reject, timer, item });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
    promise.cancel = () => {
      const pending = this.pending.get(id);
      if (!pending) return;
      clearTimeout(pending.timer);
      this.pending.delete(id);
      if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify({ id, cancel: true }));
      pending.resolve();
    };
    return promise;
  }

  call(method, params = {}) { return this.request(method, params); }
  watch(method, params, item) {
    const request = this.request(method, params, item);
    request.catch(() => this.close());
    return () => request.cancel?.();
  }
  close() { this.socket?.close(); }
}

export function applyTranscriptFrame(entries, frame) {
  if (Array.isArray(frame.reset)) return frame.reset;
  const removed = new Set(frame.remove ?? []);
  const result = entries.filter(entry => !removed.has(entry.id));
  for (const { after, entry } of frame.upsert ?? []) {
    const old = result.findIndex(row => row.id === entry.id);
    if (old >= 0) result.splice(old, 1);
    const anchor = after == null ? -1 : result.findIndex(row => row.id === after);
    if (after != null && anchor < 0) throw new Error('Transcript anchor missing');
    result.splice(anchor + 1, 0, entry);
  }
  for (const append of frame.append ?? []) {
    const entry = result.find(row => row.id === append.entry);
    const part = entry?.parts.find(row => row.id === append.part);
    if (!part || !['text', 'reasoning'].includes(part.kind)) throw new Error('Transcript part missing');
    const text = part.text + append.text;
    if (Buffer.byteLength(text) !== append.len) throw new Error('Transcript text length mismatch');
    part.text = text;
  }
  if (result.length !== frame.count) throw new Error('Transcript count mismatch');
  return result;
}
