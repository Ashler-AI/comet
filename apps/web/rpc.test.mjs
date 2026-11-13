import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { CrewRpc } from './rpc.mjs';

// Failure modes: crew-web dials the engine without its IPC bearer (every native
// Scaffold sandbox stalled at 503, 2026-11-13); it keeps a token the engine
// rotated on restart; it dials, or hangs, before the engine has published one.

// Stands in for the engine's IPC listener: 401 unless `Authorization: Bearer <token>`.
let engineToken = '';
const dials = [];
const sockets = new Set();
const engine = createServer();
engine.on('upgrade', (request, socket) => {
  dials.push(request.headers.authorization);
  if (request.headers.authorization !== `Bearer ${engineToken}`) {
    socket.end('HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n');
    return;
  }
  sockets.add(socket);
  socket.on('close', () => sockets.delete(socket));
  // The client's next frame here is its close frame; dropping the connection ends it.
  socket.once('data', () => socket.destroy());
  const accept = createHash('sha1').update(`${request.headers['sec-websocket-key']}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
});

let port;
let dataDir;
before(async () => {
  engine.listen(0, '127.0.0.1');
  await once(engine, 'listening');
  port = engine.address().port;
  dataDir = await mkdtemp(join(tmpdir(), 'crew-web-ipc-'));
});
after(async () => {
  for (const socket of sockets) socket.destroy();
  engine.close();
  await rm(dataDir, { recursive: true, force: true });
});

async function publish(token) {
  engineToken = token;
  await writeFile(join(dataDir, 'ipc-token'), `${token}\n`, { mode: 0o600 });
}

test('dials with the engine token and re-reads it after the engine restarts', async () => {
  await publish('a'.repeat(64));
  const rpc = new CrewRpc(port, { env: { COMET_DATA_DIR: dataDir } });
  await rpc.connect();
  rpc.close();
  await once(rpc, 'disconnect');
  await publish('b'.repeat(64));
  await rpc.connect();
  rpc.close();
  assert.deepEqual(dials.slice(-2), [`Bearer ${'a'.repeat(64)}`, `Bearer ${'b'.repeat(64)}`]);
});

test('fails without dialing before the engine publishes a token', async () => {
  const empty = await mkdtemp(join(tmpdir(), 'crew-web-ipc-empty-'));
  const before = dials.length;
  try {
    await assert.rejects(new CrewRpc(port, { env: { COMET_DATA_DIR: empty } }).connect(), /Crew engine unavailable/);
    await assert.rejects(new CrewRpc(port, { env: {} }).connect(), /Crew engine unavailable/);
  } finally {
    await rm(empty, { recursive: true, force: true });
  }
  assert.equal(dials.length, before);
});

test('uses the engine client token order: explicit value, then token file, then data dir', async () => {
  await publish('c'.repeat(64));
  const tokenFile = join(dataDir, 'injected-token');
  await writeFile(tokenFile, 'c'.repeat(64));
  for (const env of [
    { ZERON_IPC_TOKEN: 'c'.repeat(64), ZERON_IPC_TOKEN_FILE: join(dataDir, 'missing'), COMET_DATA_DIR: '/nonexistent' },
    { ZERON_IPC_TOKEN_FILE: tokenFile, COMET_DATA_DIR: '/nonexistent' },
  ]) {
    const rpc = new CrewRpc(port, { env });
    await rpc.connect();
    rpc.close();
  }
  assert.deepEqual(dials.slice(-2), [`Bearer ${'c'.repeat(64)}`, `Bearer ${'c'.repeat(64)}`]);
});
