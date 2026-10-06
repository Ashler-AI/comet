#!/usr/bin/env node

import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { chmod, mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { once } from "node:events";
import { createRequire } from "node:module";
import { promisify } from "node:util";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const EDGE_DIR = path.join(ROOT, "edge");
const COMET_BIN = process.env.COMET_BIN ?? path.join(ROOT, "target", "debug", "comet");
const QUIET_OWNER = process.env.COMET_SYNC_QUIET_OWNER === "1";
const MOBILE_SIMULATOR = process.env.COMET_MOBILE_SIMULATOR_ID;
const MOBILE_BUNDLE = process.env.COMET_MOBILE_BUNDLE_ID;
assert.equal(Boolean(MOBILE_SIMULATOR), Boolean(MOBILE_BUNDLE), "mobile fixture requires both simulator and bundle identity");
const SOAK_TURNS = Number(process.env.COMET_SYNC_SOAK_TURNS ?? 24);
assert.ok(Number.isSafeInteger(SOAK_TURNS) && SOAK_TURNS > 0 && SOAK_TURNS <= 2_000,
  "COMET_SYNC_SOAK_TURNS must be an integer between 1 and 2000");
const edgeRequire = createRequire(path.join(EDGE_DIR, "package.json"));
const { WebSocket } = edgeRequire("ws");
const DURABLE_SYNC_PROTOCOL = "durable-records-v1";
const OWNER_TOKEN = "sc_rc_comet_integration_owner";
const CLIENT_A_TOKEN = "sc_rc_comet_integration_client_a";
const CLIENT_B_TOKEN = "sc_rc_comet_integration_client_b";
const OWNER_SUBJECT = "owner@example.test";
const CLIENT_A_SUBJECT = "agent-a@example.test";
const CLIENT_B_SUBJECT = "agent-b@example.test";
const PROJECT_ID = "ashler-local";
const DEPLOYMENT_ID = "deployment-smoke";
const SANDBOX_ID = "smoke-001";
const LIFECYCLE_EPOCH = 1;
const DEVICE_ID = `comet-scaffold-${SANDBOX_ID}-e${LIFECYCLE_EPOCH}`;
const SESSION_ID = "11111111-1111-4111-8111-111111111111";
const CAPABILITIES = [
  "session.read",
  "session.chat",
  "session.control",
  "session.annotate",
  "session.invite",
  "session.files",
  "session.environment"
];
const REMOTE_CODE_SCOPES = [
  "remote_code:create",
  "remote_code:read",
  "remote_code:write",
  "remote_code:exec",
  "remote_code:lifecycle"
];
const STEP_TIMEOUT_MS = 15_000;
const trackedChildren = [];
let tempDir;
let scaffoldServer;
let cleaningUp = false;

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

const withTimeout = (promise, label, milliseconds = STEP_TIMEOUT_MS) => {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} timed out after ${milliseconds}ms`)), milliseconds);
    })
  ]).finally(() => clearTimeout(timer));
};

const reservePort = () =>
  withTimeout(
    new Promise((resolve, reject) => {
      const server = net.createServer();
      server.once("error", reject);
      server.listen(0, "127.0.0.1", () => {
        const address = server.address();
        assert.ok(address && typeof address === "object");
        const { port } = address;
        server.close((error) => (error ? reject(error) : resolve(port)));
      });
    }),
    "reserve local port"
  );

const waitFor = async (label, probe, milliseconds = STEP_TIMEOUT_MS) => {
  const deadline = Date.now() + milliseconds;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const result = await probe();
      if (result) return result;
    } catch (error) {
      lastError = error;
    }
    await delay(100);
  }
  throw new Error(`${label} timed out${lastError ? `: ${lastError.message}` : ""}`);
};

const readBody = async (request) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
};

const sendJson = (response, status, value) => {
  response.writeHead(status, {
    "content-type": "application/json",
    "cache-control": "no-store"
  });
  response.end(JSON.stringify(value));
};

const startFakeScaffold = async (port) => {
  const origin = `http://127.0.0.1:${port}`;
  const observations = { sessionChecks: 0, targetProofs: 0 };
  const server = createServer(async (request, response) => {
    try {
      const authorization = request.headers.authorization ?? "";
      const token = authorization.startsWith("Bearer ") ? authorization.slice(7) : "";
      const subjectByToken = {
        [OWNER_TOKEN]: OWNER_SUBJECT,
        [`${OWNER_SUBJECT}@${PROJECT_ID}`]: OWNER_SUBJECT,
        [CLIENT_A_TOKEN]: CLIENT_A_SUBJECT,
        [CLIENT_B_TOKEN]: CLIENT_B_SUBJECT
      };
      const actorSubject = subjectByToken[token];
      if (!actorSubject) {
        sendJson(response, 401, { error: "unauthenticated" });
        return;
      }
      if (request.method === "GET" && request.url === "/api/code-sandboxes/auth/session") {
        observations.sessionChecks += 1;
        sendJson(response, 200, {
          ok: true,
          resource: origin,
          actor: { sub: actorSubject, auth: "iap" },
          scopes: REMOTE_CODE_SCOPES
        });
        return;
      }
      if (
        request.method === "POST" &&
        request.url === `/api/code-sandboxes/${SANDBOX_ID}/comet-target/verify`
      ) {
        assert.equal(token, OWNER_TOKEN, "only the owner bearer may prove the sandbox target");
        const target = JSON.parse(await readBody(request));
        assert.deepEqual(target, {
          projectId: PROJECT_ID,
          sandboxId: SANDBOX_ID,
          deploymentId: DEPLOYMENT_ID,
          targetDeviceId: DEVICE_ID,
          sessionId: SESSION_ID,
          lifecycleEpoch: LIFECYCLE_EPOCH
        });
        observations.targetProofs += 1;
        sendJson(response, 200, {
          ok: true,
          profile: {
            version: "scaffold.comet-runtime.v1",
            ...target,
            actor: { sub: OWNER_SUBJECT }
          }
        });
        return;
      }
      sendJson(response, 404, { error: "not_found" });
    } catch (error) {
      sendJson(response, 500, { error: error.message });
    }
  });
  await withTimeout(
    new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(port, "127.0.0.1", resolve);
    }),
    "start fake Scaffold"
  );
  return { server, origin, observations };
};

const captureOutput = (child, label) => {
  const lines = [];
  const capture = (chunk) => {
    lines.push(...chunk.toString("utf8").split(/\r?\n/).filter(Boolean));
    if (lines.length > 80) lines.splice(0, lines.length - 80);
  };
  child.stdout?.on("data", capture);
  child.stderr?.on("data", capture);
  child.on("error", (error) => {
    child.spawnError = error;
    capture(error.stack ?? error.message);
  });
  child.once("exit", (code, signal) => capture(`${new Date().toISOString()} ${label} exited code=${code} signal=${signal}`));
  child.outputSummary = () => `${label} output:\n${lines.join("\n")}`;
};

const spawnTracked = (label, command, args, options) => {
  const child = spawn(command, args, {
    ...options,
    detached: process.platform !== "win32",
    stdio: ["ignore", "pipe", "pipe"]
  });
  captureOutput(child, label);
  trackedChildren.push(child);
  return child;
};

const terminateChild = async (child) => {
  if (child.spawnError || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once("exit", resolve));
  try {
    if (process.platform === "win32") child.kill("SIGTERM");
    else process.kill(-child.pid, "SIGTERM");
  } catch {
    child.kill("SIGTERM");
  }
  if (await Promise.race([exited.then(() => true), delay(2_000).then(() => false)])) return;
  try {
    if (process.platform === "win32") child.kill("SIGKILL");
    else process.kill(-child.pid, "SIGKILL");
  } catch {
    child.kill("SIGKILL");
  }
  await Promise.race([exited, delay(1_000)]);
};

const crashChild = async (child) => {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  if (process.platform === "win32") child.kill("SIGKILL");
  else process.kill(-child.pid, "SIGKILL");
  await withTimeout(exited, "crashed fixture process exit", 10_000);
};

const residentKiB = async (child) => {
  if (process.platform === "linux") {
    const status = await readFile(`/proc/${child.pid}/status`, "utf8");
    const rss = status.match(/^VmRSS:\s+(\d+)\s+kB$/m);
    assert.ok(rss, "headless fixture RSS is available");
    return Number(rss[1]);
  }
  const { stdout } = await promisify(execFile)("ps", ["-p", String(child.pid), "-o", "rss="]);
  const rss = Number(stdout.trim());
  assert.ok(Number.isFinite(rss) && rss > 0, "headless fixture RSS is available");
  return rss;
};

const cleanup = async () => {
  if (cleaningUp) return;
  cleaningUp = true;
  await Promise.allSettled(trackedChildren.map(terminateChild));
  if (scaffoldServer) {
    await Promise.race([
      new Promise((resolve) => scaffoldServer.close(resolve)),
      delay(1_000)
    ]);
  }
  if (tempDir) await rm(tempDir, { recursive: true, force: true });
};

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    void cleanup().finally(() => process.exit(128 + (signal === "SIGINT" ? 2 : 15)));
  });
}

const fetchJson = async (url, init = {}) => {
  const response = await fetch(url, {
    ...init,
    signal: AbortSignal.timeout(STEP_TIMEOUT_MS)
  });
  const text = await response.text();
  let body;
  try {
    body = text ? JSON.parse(text) : undefined;
  } catch {
    body = text;
  }
  return { response, body };
};

const ownerFetch = (edgeOrigin, pathname, init = {}) =>
  fetchJson(`${edgeOrigin}${pathname}${pathname.includes("?") ? "&" : "?"}syncProtocol=${DURABLE_SYNC_PROTOCOL}`, {
    ...init,
    headers: {
      authorization: `Bearer ${OWNER_TOKEN}`,
      ...(init.body ? { "content-type": "application/json" } : {}),
      ...init.headers
    }
  });

const openWebSocket = (url, label) =>
  withTimeout(
    new Promise((resolve, reject) => {
      const target = new URL(url);
      if (target.pathname !== "/") target.searchParams.set("syncProtocol", DURABLE_SYNC_PROTOCOL);
      const socket = new WebSocket(target);
      socket.binaryType = "arraybuffer";
      const onOpen = () => {
        dispose();
        resolve(socket);
      };
      const onError = (event) => {
        dispose();
        const detail = event.error?.message ?? event.message ?? "network error";
        reject(new Error(`${label} failed to open: ${detail}`));
      };
      const onClose = (event) => {
        dispose();
        reject(new Error(`${label} closed during handshake (${event.code} ${event.reason})`));
      };
      const dispose = () => {
        socket.removeEventListener("open", onOpen);
        socket.removeEventListener("error", onError);
        socket.removeEventListener("close", onClose);
      };
      socket.addEventListener("open", onOpen);
      socket.addEventListener("error", onError);
      socket.addEventListener("close", onClose);
    }),
    label, 30_000
  );

const closeWebSocket = (socket, label) => {
  if (socket.readyState === WebSocket.CLOSED) return Promise.resolve();
  return new Promise((resolve) => {
    let timer;
    const finish = () => {
      clearTimeout(timer);
      socket.removeEventListener("close", finish);
      resolve();
    };
    socket.addEventListener("close", finish);
    socket.close(1000, label);
    // Miniflare can retain one side of a WebSocketPair in CLOSING indefinitely.
    // DeviceRoom explicitly supersedes the old logical connection on reconnect.
    timer = setTimeout(finish, 250);
  });
};

const encodeDeviceFrame = (header, payload) => {
  const headerBytes = Buffer.from(JSON.stringify(header), "utf8");
  const prefix = [];
  let length = headerBytes.length;
  do {
    let byte = length & 0x7f;
    length >>>= 7;
    if (length) byte |= 0x80;
    prefix.push(byte);
  } while (length);
  return Buffer.concat([Buffer.from(prefix), headerBytes, Buffer.from(payload)]);
};

const decodeDeviceFrame = (value) => {
  const bytes = Buffer.from(value);
  let offset = 0;
  let length = 0;
  let shift = 0;
  for (;;) {
    const byte = bytes[offset++];
    assert.notEqual(byte, undefined, "truncated device frame length");
    length |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) break;
    shift += 7;
    assert.ok(shift < 32, "device frame length overflow");
  }
  const headerEnd = offset + length;
  assert.ok(headerEnd <= bytes.length, "truncated device frame header");
  return {
    header: JSON.parse(bytes.subarray(offset, headerEnd).toString("utf8")),
    payload: bytes.subarray(headerEnd)
  };
};

const rpcCall = (socket, id, method, params = {}) =>
  withTimeout(
    new Promise((resolve, reject) => {
      const onMessage = (event) => {
        try {
          if (!(event.data instanceof ArrayBuffer)) return;
          const { header, payload } = decodeDeviceFrame(event.data);
          if (header.k === " relay") {
            const control = JSON.parse(payload.toString("utf8"));
            dispose();
            reject(new Error(`relay rejected RPC: ${control.error}`));
            return;
          }
          if (header.k !== "rpc") return;
          for (const line of payload.toString("utf8").split("\n")) {
            if (!line.trim()) continue;
            const reply = JSON.parse(line);
            if (reply.id !== id) continue;
            dispose();
            if (Object.hasOwn(reply, "err")) reject(new Error(reply.err));
            else if (Object.hasOwn(reply, "ok")) resolve(reply.ok);
            else reject(new Error(`unexpected RPC reply: ${line}`));
            return;
          }
        } catch (error) {
          dispose();
          reject(error);
        }
      };
      const onClose = (event) => {
        dispose();
        reject(new Error(`RPC socket closed (${event.code} ${event.reason})`));
      };
      const dispose = () => {
        socket.removeEventListener("message", onMessage);
        socket.removeEventListener("close", onClose);
      };
      socket.addEventListener("message", onMessage);
      socket.addEventListener("close", onClose);
      socket.send(
        encodeDeviceFrame(
          { s: "rpc", k: "rpc" },
          Buffer.from(JSON.stringify({ id, method, params }), "utf8")
        )
      );
    }),
    `${method} RPC`
  );

const expectRelayDenial = (socket, id, method, params = {}) =>
  withTimeout(
    new Promise((resolve, reject) => {
      const onMessage = (event) => {
        try {
          if (!(event.data instanceof ArrayBuffer)) return;
          const { header, payload } = decodeDeviceFrame(event.data);
          if (header.k !== " relay") return;
          const { error } = JSON.parse(payload.toString("utf8"));
          dispose();
          resolve(error);
        } catch (error) {
          dispose();
          reject(error);
        }
      };
      const onClose = (event) => {
        dispose();
        resolve(`socket_closed:${event.code}`);
      };
      const dispose = () => {
        socket.removeEventListener("message", onMessage);
        socket.removeEventListener("close", onClose);
      };
      socket.addEventListener("message", onMessage);
      socket.addEventListener("close", onClose);
      socket.send(
        encodeDeviceFrame(
          { s: "rpc", k: "rpc" },
          Buffer.from(JSON.stringify({ id, method, params }), "utf8")
        )
      );
    }),
    `${method} relay denial`
  );

const localRpc = async (port, method, params = {}) => {
  const socket = await openWebSocket(`ws://127.0.0.1:${port}`, "local IPC");
  try {
    return await withTimeout(new Promise((resolve, reject) => {
      socket.addEventListener("message", (event) => {
        const reply = JSON.parse(event.data);
        if (reply.id !== 1) return;
        if (Object.hasOwn(reply, "err")) reject(new Error(`${method} local RPC (${params.command?.kind ?? "read"}): ${reply.err}`));
        else if (Object.hasOwn(reply, "ok")) resolve(reply.ok);
        else if (Object.hasOwn(reply, "item")) resolve(reply.item);
      });
      socket.send(JSON.stringify({ id: 1, method, params }));
    }), `${method} local RPC (${params.command?.kind ?? "read"})`);
  } finally { await closeWebSocket(socket, "local IPC"); }
};

const seedWorkspaceHistory = async (edgeOrigin, ports) => {
  const { LoroWebsocketClient } = await import(edgeRequire.resolve("loro-websocket"));
  const { LoroAdaptor } = await import(edgeRequire.resolve("loro-adaptors/loro"));
  const { LoroMap } = edgeRequire("loro-crdt");
  const client = new LoroWebsocketClient({
    url: `${edgeOrigin.replace("http:", "ws:")}/workspace/${PROJECT_ID}/ws?token=${OWNER_TOKEN}&syncProtocol=${DURABLE_SYNC_PROTOCOL}`
  });
  const adaptor = new LoroAdaptor();
  const fixtureDeviceId = crypto.randomUUID();
  const ids = Array.from({ length: 1_600 }, () => crypto.randomUUID());
  try {
    await withTimeout(client.waitConnected(), "workspace fixture transport");
    await withTimeout(client.join({ roomId: `ws4/${PROJECT_ID}`, crdtAdaptor: adaptor }), "workspace fixture join");
    await withTimeout(adaptor.waitForReachingServerVersion(), "workspace fixture backfill");
    const doc = adaptor.getDoc();
    const at = Date.now();
    for (const [index, id] of ids.entries()) {
      const chat = doc.getMap("chats").setContainer(id, new LoroMap());
      for (const [key, value] of Object.entries({
        id, deviceId: fixtureDeviceId, title: `Crew reliability history ${index}`,
        archived: false, createdAt: at, lastMessageAt: at,
        lastMessagePreview: "Preserved historical session", cwd: tempDir,
        config: { harness: "mock", model: "fable-5", sandbox: "workspace-write" }
      })) chat.set(key, value);
      const ref = doc.getMap("sessionRefs").setContainer(
        `${Buffer.byteLength(OWNER_SUBJECT)}:${OWNER_SUBJECT}:${id}`, new LoroMap());
      ref.set("userId", OWNER_SUBJECT);
      ref.set("chatId", id);
      ref.set("addedAt", at);
    }
    doc.commit();
    for (const port of ports) {
      await waitFor("large workspace materialized on native client", async () => {
        const chats = await localRpc(port, "WatchChats");
        const imported = chats.filter((chat) => chat.deviceId === fixtureDeviceId);
        if (imported.length !== ids.length) return false;
        assert.deepEqual(new Set(imported.map((chat) => chat.id)), new Set(ids));
        return true;
      }, 30_000);
    }
  } finally { client.close(); }
  return { fixtureDeviceId, ids };
};

const resetWorkspaceEpoch = async (edgeOrigin, ports, history, offlineChatId, chatId, remoteTitle) => {
  const { LoroWebsocketClient } = await import(edgeRequire.resolve("loro-websocket"));
  const { LoroAdaptor } = await import(edgeRequire.resolve("loro-adaptors/loro"));
  const { LoroDoc, LoroMap } = edgeRequire("loro-crdt");
  const reader = new LoroWebsocketClient({ url: `${edgeOrigin.replace("http:", "ws:")}/workspace/${PROJECT_ID}/ws?token=${OWNER_TOKEN}&syncProtocol=${DURABLE_SYNC_PROTOCOL}` });
  const adaptor = new LoroAdaptor();
  try {
    await withTimeout(reader.waitConnected(), "reset seed reader");
    await withTimeout(reader.join({ roomId: `ws4/${PROJECT_ID}`, crdtAdaptor: adaptor }), "reset seed join");
    await withTimeout(adaptor.waitForReachingServerVersion(), "reset seed snapshot");
    const seed = new LoroDoc();
    try {
      for (const [name, values] of Object.entries(adaptor.getDoc().toJSON())) {
        assert.ok(values && typeof values === "object" && !Array.isArray(values), `workspace root ${name} is a map`);
        const root = seed.getMap(name);
        for (const [key, value] of Object.entries(values)) {
          if (name !== "meta" && value && typeof value === "object" && !Array.isArray(value)) {
            const row = root.setContainer(key, new LoroMap());
            for (const [field, data] of Object.entries(value)) row.set(field, data);
          } else root.set(key, value);
        }
      }
      seed.getMap("chats").get(history.ids[0]).set("title", "Crew authoritative epoch seed");
      seed.commit();
      const result = await ownerFetch(edgeOrigin, `/workspace/${PROJECT_ID}/reset-log`, {
        method: "POST", headers: { "content-type": "application/octet-stream" }, body: seed.export({ mode: "snapshot" }),
      });
      assert.equal(result.response.status, 200, "authenticated epoch reset accepts the complete seed");
      for (const port of ports) await waitFor("native client adopts independent epoch seed", async () => {
        const rows = await localRpc(port, "WatchChats");
        return rows.find((row) => row.id === history.ids[0])?.title === "Crew authoritative epoch seed"
          && rows.find((row) => row.id === offlineChatId)?.title === "Crew offline creation survives crash"
          && rows.find((row) => row.id === chatId)?.title === remoteTitle
          && rows.filter((row) => row.deviceId === history.fixtureDeviceId).length === history.ids.length
          && (await localRpc(port, "SyncStatus")).workspace?.connected === true;
      }, 30_000);
      console.log("PASS authenticated independent workspace epoch reset preserves accepted creation, rename and all history rows");
    } finally { seed.free(); }
  } finally { reader.close(); }
};

const retiredProtocolSmoke = async (edgeOrigin, chatId, deviceId) => {
  const { LoroWebsocketClient } = await import(edgeRequire.resolve("loro-websocket"));
  const { LoroAdaptor } = await import(edgeRequire.resolve("loro-adaptors/loro"));
  const { LoroDoc } = edgeRequire("loro-crdt");
  const { CrdtType, MessageType, UpdateStatusCode, encode, decode } = edgeRequire("loro-protocol");
  const oldURL = `${edgeOrigin.replace("http:", "ws:")}/session/${chatId}/ws?token=${OWNER_TOKEN}`;
  const reader = new LoroWebsocketClient({ url: oldURL });
  const adaptor = new LoroAdaptor();
  const join = adaptor.handleJoinOk.bind(adaptor);
  adaptor.handleJoinOk = async (response) => {
    assert.equal(response.permission, "read", "retired protocol receives only read permission");
    return join(response);
  };
  let socket;
  const attempt = new LoroDoc();
  try {
    await withTimeout(reader.waitConnected(), "retired reader transport");
    await withTimeout(reader.join({ roomId: chatId, crdtAdaptor: adaptor }), "retired reader join");
    await withTimeout(adaptor.waitForReachingServerVersion(), "retired reader backfill");
    const expectedIds = adaptor.getDoc().toJSON().messages.map((entry) => entry.id);
    assert.ok(expectedIds.some((id) => typeof id === "string"), "retired reader receives accepted transcript identities");
    socket = new WebSocket(oldURL);
    socket.binaryType = "arraybuffer";
    await withTimeout(once(socket, "open"), "retired write probe transport", 30_000);
    const replies = [];
    socket.addEventListener("message", (event) => replies.push(decode(new Uint8Array(event.data))));
    socket.send(encode({ type: MessageType.JoinRequest, crdt: CrdtType.Loro, roomId: chatId,
      auth: new Uint8Array(), version: new Uint8Array() }));
    const joined = await waitFor("retired raw read-only join", async () => replies.find((reply) => reply.type === MessageType.JoinResponseOk));
    assert.equal(joined.permission, "read");
    attempt.getMap("meta").set("retiredWriteProbe", true); attempt.commit();
    const batchId = "0x000000000000f135";
    socket.send(encode({ type: MessageType.DocUpdate, crdt: CrdtType.Loro, roomId: chatId,
      batchId, updates: [attempt.export({ mode: "snapshot" })] }));
    const denied = await waitFor("retired raw write rejection", async () => replies.find((reply) => reply.type === MessageType.Ack && reply.refId === batchId));
    assert.equal(denied.status, UpdateStatusCode.PermissionDenied);
    const snapshot = await fetch(`${edgeOrigin}/snapshot/${chatId}`, { headers: { authorization: `Bearer ${OWNER_TOKEN}` } });
    assert.equal(snapshot.status, 200);
    const persisted = new LoroDoc();
    try {
      persisted.import(new Uint8Array(await snapshot.arrayBuffer()));
      assert.notEqual(persisted.toJSON().meta.retiredWriteProbe, true, "rejected old write never persists");
      assert.deepEqual(persisted.toJSON().messages.map((entry) => entry.id), expectedIds);
    } finally { persisted.free(); }
    for (const role of ["host", "client"]) {
      const legacyRelay = `${edgeOrigin.replace("http:", "ws:")}/device/${deviceId}/ws?role=${role}&token=${OWNER_TOKEN}`;
      const status = await withTimeout(new Promise((resolve, reject) => {
        const probe = new WebSocket(legacyRelay);
        probe.once("unexpected-response", (_request, response) => {
          response.resume(); response.once("end", () => { resolve(response.statusCode); probe.terminate(); });
        });
        probe.once("open", () => { probe.close(); reject(new Error(`retired ${role} relay registered`)); });
        probe.on("error", reject);
      }), `retired ${role} relay rejection`, 30_000);
      assert.equal(status, 426, "retired relay must require Crew update before registration/control");
    }
    console.log("PASS retired protocol reads accepted history but cannot publish or register/control a host");
  } finally {
    if (socket) await closeWebSocket(socket, "retired reader probe");
    reader.close(); attempt.free();
  }
};

const liveMobileSmoke = async (edgeOrigin, deviceId, workspacePath) => {
  if (!MOBILE_SIMULATOR) return;
  assert.equal(process.platform, "darwin", "mobile convergence requires the owned macOS simulator");
  const exec = promisify(execFile);
  await exec("xcrun", ["simctl", "bootstatus", MOBILE_SIMULATOR, "-b"], { timeout: 300_000 });
  const { stdout } = await exec("xcrun", ["simctl", "get_app_container", MOBILE_SIMULATOR, MOBILE_BUNDLE, "data"]);
  const log = path.join(stdout.trim(), "Documents", "e2e.log");
  await rm(log, { force: true });
  await exec("xcrun", ["simctl", "launch", "--terminate-running-process", MOBILE_SIMULATOR, MOBILE_BUNDLE, "-e2e"], {
    env: { ...process.env, SIMCTL_CHILD_CREW_E2E_EDGE_URL: edgeOrigin,
      SIMCTL_CHILD_CREW_E2E_ACCESS_TOKEN: OWNER_TOKEN,
      SIMCTL_CHILD_CREW_E2E_USER_ID: OWNER_SUBJECT, SIMCTL_CHILD_CREW_E2E_PROJECT_SCOPE: PROJECT_ID,
      SIMCTL_CHILD_CREW_E2E_DEVICE_ID: deviceId, SIMCTL_CHILD_CREW_E2E_WORKSPACE_PATH: workspacePath },
  });
  await waitFor("mobile/native/Edge live command and transcript convergence", async () => {
    const text = await readFile(log, "utf8").catch((error) => { if (error.code === "ENOENT") return ""; throw error; });
    assert.ok(!/\bFAIL\b/.test(text), `mobile live convergence failed:\n${text}`);
    if (!/^\[\d+\] done$/m.test(text)) return false;
    for (const marker of ["OK workspace synced", "OK relay ListFolders", "OK relay ListModels", "OK run admitted", "OK transcript streamed"])
      assert.ok(text.includes(marker), `live mobile convergence misses ${marker}`);
    console.log(text);
    return true;
  }, 120_000);
  console.log("PASS real mobile/native/Edge transport convergence; deterministic mock inference");
};

const ordinaryDeviceSmoke = async (edgeOrigin, scaffoldOrigin, restartEdge) => {
  const ports = [await reservePort(), await reservePort()];
  const devices = [];
  const startDevice = async (index) => {
    const dataDir = path.join(tempDir, `ordinary-${index}`);
    await mkdir(dataDir, { recursive: true });
    await writeFile(path.join(dataDir, "session.json"), JSON.stringify({
      accessToken: OWNER_TOKEN, user: { id: OWNER_SUBJECT, email: OWNER_SUBJECT, name: "Owner" },
      projectScope: PROJECT_ID, capabilities: CAPABILITIES
    }), { mode: 0o600 });
    const child = spawnTracked(`ordinary Crew device ${index}`, COMET_BIN,
      ["headless", "--edge-url", edgeOrigin], { cwd: ROOT, env: {
        ...process.env, COMET_DATA_DIR: dataDir, COMET_IPC_PORT: String(ports[index]),
        COMET_PROJECT_SCOPE: PROJECT_ID, COMET_SCAFFOLD_URL: scaffoldOrigin,
        COMET_HARNESS: "mock", COMET_MOCK_REPEAT: QUIET_OWNER ? "1000" : "30", COMET_MOCK_DELAY_MS: "200",
        ASHLER_INCREMENTAL_TSC_CHECKS: "false", RUST_LOG: "info"
      } });
    devices[index] = child;
    await waitFor(`ordinary device ${index} IPC`, async () => {
      if (child.spawnError || child.exitCode !== null) throw new Error(child.outputSummary());
      return localRpc(ports[index], "LocalDevice");
    });
  };
  for (const index of [0, 1]) await startDevice(index);
  const [desktop, devbox] = ports;
  const { deviceId } = await localRpc(devbox, "LocalDevice");
  await waitFor("ordinary Devbox relay", async () => {
    const result = await ownerFetch(edgeOrigin, `/device/${deviceId}/status`);
    return result.body?.hostConnected;
  });
  const chatId = crypto.randomUUID();
  const spaceId = crypto.randomUUID();
  await localRpc(devbox, "Mutate", { op: "createSpace", spaceId, deviceId, path: tempDir });
  await localRpc(devbox, "Mutate", { op: "createChat", chatId, spaceId, config: {
    harness: "mock", model: "fable-5", reasoning: null, sandbox: "workspace-write"
  } });
  await waitFor("remote workspace ownership sync", async () =>
    (await localRpc(desktop, "WatchChats")).some((chat) => chat.id === chatId && chat.deviceId === deviceId));
  const request = { prompt: "remote smoke", model: null, reasoning: null, cwd: tempDir,
    sandbox: "workspace-write", resume: null };
  const send = (command) => localRpc(desktop, "QueueCommand", { chatId, commandId: crypto.randomUUID(), command });
  await send({ kind: "run", messageId: crypto.randomUUID(), request });
  await waitFor("remote legacy response synced to desktop", async () =>
    JSON.stringify(await localRpc(desktop, "WatchDocMessages", { chatId })).includes("Streaming pipeline"));
  await send({ kind: "interrupt" });
  await waitFor("legacy remote stop", async () =>
    (await localRpc(devbox, "WatchSessions")).some((session) => session.chatId === chatId && session.status === "idle"));
  const sessionId = crypto.randomUUID();
  const typed = (action) => ({ kind: "control", source: "local", sessionId,
    ownerDeviceId: deviceId, actorDeviceId: "desktop", actorSubject: OWNER_SUBJECT, grantId: "", action });
  const typedStart = await send(typed({ action: "start", message_id: crypto.randomUUID(), request }));
  try {
    await waitFor("ordinary Local session publication", async () =>
      (await localRpc(desktop, "WatchCollaboration", { chatId })).sessions.some((session) =>
        session.sessionId === sessionId && session.ownerDeviceId === deviceId && session.source === "local"));
  } catch (error) {
    const outcome = await localRpc(devbox, "ReadSessionCommand", { chatId, commandId: typedStart.commandId });
    throw new Error(`${error.message}; owner command status=${outcome.command?.status ?? "missing"}, resolution=${String(outcome.command?.resolution ?? "none").slice(0, 500)}`);
  }
  await send(typed({ action: "steer", prompt: "continue", message_id: crypto.randomUUID() }));
  await send(typed({ action: "stop" }));
  await waitFor("typed remote stop", async () =>
    (await localRpc(devbox, "WatchCollaboration", { chatId })).sessions.some((session) =>
      session.sessionId === sessionId && session.status === "idle"));
  const readParams = { chatId, commandId: typedStart.commandId, targetDeviceId: deviceId, roomProjection: null };
  const outcome = await localRpc(desktop, "ReadSessionCommand", readParams);
  assert.equal(outcome.command?.status, "applied", "the observing device reads the original owner's durable outcome");
  assert.equal(outcome.command?.payload.sessionId, sessionId);
  for (const token of [OWNER_TOKEN, CLIENT_A_TOKEN]) {
    const unscoped = await openWebSocket(
      `${edgeOrigin.replace("http:", "ws:")}/device/${deviceId}/ws?role=client&token=${token}`,
      "unscoped outcome reader"
    );
    try {
      await assert.rejects(rpcCall(unscoped, 1, "ReadSessionCommand", {
        chatId, commandId: typedStart.commandId
      }), { message: "peer_command_scope_denied" });
    } finally { await closeWebSocket(unscoped, "unscoped outcome reader"); }
  }
  const attacker = await openWebSocket(
    `${edgeOrigin.replace("http:", "ws:")}/device/${deviceId}/ws?role=client&purpose=control&controlSessionId=${chatId}&token=${CLIENT_A_TOKEN}`,
    "foreign principal control"
  );
  await assert.rejects(rpcCall(attacker, 1, "AdmitPeerCommand", {
    chatId, commandId: crypto.randomUUID(), command: { kind: "interrupt" }
  }), { message: "peer_command_scope_denied" });
  if (QUIET_OWNER) {
    await send({ kind: "run", messageId: crypto.randomUUID(), request });
    await waitFor("observer sees quiet active owner", async () =>
      (await localRpc(desktop, "WatchCollaboration", { chatId })).sessions.some((row) => row.sessionId === chatId && row.status === "working"));
    await delay(50_000);
    const owner = (await localRpc(desktop, "WatchCollaboration", { chatId })).sessions.find((row) => row.sessionId === chatId);
    assert.equal(owner?.status, "working", "quiet owner stays active beyond the freshness lease");
    assert.ok(Date.now() - owner.updatedAt <= 45_000, "observer consumes genuine owner register heartbeats");
    await send({ kind: "interrupt" });
    await waitFor("quiet owner stops", async () =>
      (await localRpc(devbox, "WatchSessions")).some((row) => row.chatId === chatId && row.status === "idle"));
    console.log("PASS durable owner register freshness beyond the 45-second lease");
  }
  await closeWebSocket(attacker, "foreign principal");
  console.log("PASS ordinary two-device legacy start/response/stop and Local typed start/steer/stop; foreign principal denied");

  const history = await seedWorkspaceHistory(edgeOrigin, ports);
  const { deviceId: desktopId } = await localRpc(desktop, "LocalDevice");
  const offlineChatId = crypto.randomUUID();
  const offlineSpaceId = crypto.randomUUID();
  const recoveredTitle = "Crew offline creation survives crash";
  const remoteTitle = "Crew concurrent Devbox rename survives restart";
  const reconnectStartedAt = await restartEdge(async () => {
    for (const port of ports) await waitFor("workspace reports disconnected", async () =>
      (await localRpc(port, "SyncStatus")).workspace?.connected === false, 30_000);
    await localRpc(desktop, "Mutate", { op: "createSpace", spaceId: offlineSpaceId,
      deviceId: desktopId, path: tempDir });
    await localRpc(desktop, "Mutate", { op: "createChat", chatId: offlineChatId,
      spaceId: offlineSpaceId });
    await localRpc(desktop, "Mutate", { op: "renameChat", chatId: offlineChatId, title: recoveredTitle });
    await localRpc(devbox, "Mutate", { op: "renameChat", chatId, title: remoteTitle });
    await crashChild(devices[0]);
    await startDevice(0);
    const restored = (await localRpc(desktop, "WatchChats")).find((chat) => chat.id === offlineChatId);
    assert.equal(restored?.title, recoveredTitle, "offline creation and rename persist before the RPC receipt");
  });
  for (const port of ports) {
    await waitFor("workspace converges after simultaneous offline branches", async () => {
      const chats = await localRpc(port, "WatchChats");
      return chats.find((chat) => chat.id === offlineChatId)?.title === recoveredTitle
        && chats.find((chat) => chat.id === chatId)?.title === remoteTitle
        && chats.filter((chat) => chat.deviceId === history.fixtureDeviceId).length === history.ids.length
        && (await localRpc(port, "SyncStatus")).workspace?.connected === true;
    }, 30_000);
  }
  const catchupMs = Date.now() - reconnectStartedAt;
  assert.ok(catchupMs <= 30_000, `workspace catch-up took ${catchupMs}ms`);
  await waitFor("owner relay is available after workspace outage", async () =>
    (await ownerFetch(edgeOrigin, `/device/${deviceId}/status`)).body?.hostConnected, 30_000);
  const rss = [];
  for (let turn = 0; turn < SOAK_TURNS; turn++) {
    const commandId = crypto.randomUUID();
    const messageId = crypto.randomUUID();
    const queued = { chatId, commandId, command: { kind: "run", messageId, request } };
    if (turn === 0) {
      const uncertain = await openWebSocket(`ws://127.0.0.1:${desktop}`, "uncertain admission IPC");
      try {
        let admissionError;
        uncertain.addEventListener("message", (event) => {
          const reply = JSON.parse(event.data);
          if (reply.id === 1 && Object.hasOwn(reply,"err")) admissionError = reply.err;
        });
        uncertain.send(JSON.stringify({ id: 1, method: "QueueCommand", params: queued }));
        await waitFor("admission succeeded without consuming its reply", async () => {
          if (admissionError) throw new Error(`unconsumed admission failed: ${admissionError}`);
          return (await localRpc(devbox, "WatchDocMessages", { chatId })).reset.some((entry) =>
            entry.id === messageId && entry.role === "user");
        }, 30_000);
      } finally { await closeWebSocket(uncertain, "discard admission acknowledgement"); }
    }
    const first = await localRpc(desktop, "QueueCommand", queued);
    assert.deepEqual(await localRpc(desktop, "QueueCommand", queued), first,
      "lost admission acknowledgement retry retains the same command receipt");
    await waitFor("durable user send reaches its owning host once", async () => {
      const snapshot = await localRpc(devbox, "WatchDocMessages", { chatId });
      const matches = snapshot.reset.filter((entry) => entry.id === messageId);
      assert.ok(matches.length <= 1, "a retry must not duplicate the user message");
      return matches[0]?.role === "user";
    }, 30_000);
    if (turn === 0) {
      await restartEdge(async () => {
        await crashChild(devices[1]);
        await startDevice(1);
      });
      await waitFor("owner relay reconnects before a new control", async () =>
        (await ownerFetch(edgeOrigin, `/device/${deviceId}/status`)).body?.hostConnected, 30_000);
      for (const port of ports) await waitFor("accepted send survives publisher and edge crash", async () => {
        const snapshot = await localRpc(port, "WatchDocMessages", { chatId });
        const matches = snapshot.reset.filter((entry) => entry.id === messageId);
        assert.ok(matches.length <= 1, "crash recovery must not duplicate the accepted send");
        return matches[0]?.role === "user";
      }, 30_000);
    }
    await send({ kind: "interrupt" });
    try {
      await waitFor("owner completion reaches observing desktop", async () =>
        (await localRpc(desktop, "WatchSessions")).some((session) =>
          session.chatId === chatId && ["idle", "errored"].includes(session.status)), 30_000);
    } catch (error) {
      const [owner, observer, collaboration] = await Promise.all([
        localRpc(devbox,"WatchSessions"),localRpc(desktop,"WatchSessions"),localRpc(devbox,"WatchCollaboration",{chatId}),
      ]);
      throw new Error(`${error.message}; turn=${turn}; owner=${JSON.stringify(owner)}; observer=${JSON.stringify(observer)}; collaboration=${JSON.stringify(collaboration)}`);
    }
    if (turn >= 3) rss.push(await residentKiB(devices[1]));
  }
  if (rss.length > 1) {
    const growthKiB = Math.max(...rss) - rss[0];
    assert.ok(growthKiB <= 128 * 1_024, `headless RSS grew ${growthKiB}KiB after warmup`);
  }
  await resetWorkspaceEpoch(edgeOrigin, ports, history, offlineChatId, chatId, remoteTitle);
  await retiredProtocolSmoke(edgeOrigin, chatId, deviceId);
  await liveMobileSmoke(edgeOrigin, (await localRpc(devbox, "LocalDevice")).deviceId, await realpath(tempDir));
  console.log(JSON.stringify({ checks: ["large-workspace", "edge-crash", "offline-create-rename",
    "local-crash-after-receipt", "concurrent-branch-convergence", "lost-admission-acknowledgement",
    "admission-retry-deduplication", "publisher-and-edge-crash", "owner-completion",
    ...(rss.length > 1 ? ["bounded-rss"] : [])], historyRows: history.ids.length,
    catchupMs, turns: SOAK_TURNS, rssKiB: rss, quietOwner: QUIET_OWNER }));
};

const main = async () => {
  tempDir = await mkdtemp(path.join(os.tmpdir(), "comet-integration-smoke-"));
  const scaffoldPort = await reservePort();
  const fake = await startFakeScaffold(scaffoldPort);
  scaffoldServer = fake.server;
  const edgePort = await reservePort();
  const edgeOrigin = `http://127.0.0.1:${edgePort}`;
  const wrangler = path.join(EDGE_DIR, "node_modules", ".bin", "wrangler");
  let worker;
  const startEdge = async () => {
    worker = spawnTracked(
    "local Edge Worker",
    wrangler,
    [
      "dev",
      "--local",
      "--ip",
      "127.0.0.1",
      "--port",
      String(edgePort),
      "--inspector-port",
      "0",
      "--persist-to",
      path.join(tempDir, "worker-state"),
      "--var",
      "AUTH_MODE:scaffold",
      "--var",
      "ENVIRONMENT:local",
      "--var",
      `SCAFFOLD_CONTROL_PLANE_URL:${fake.origin}`,
      "--var",
      `SCAFFOLD_PROJECT_SCOPE:${PROJECT_ID}`,
      "--var",
      `SCAFFOLD_REQUIRED_CAPABILITIES:${CAPABILITIES.join(" ")}`
    ],
      { cwd: EDGE_DIR, env: { ...process.env, NO_COLOR: "1", ASHLER_INCREMENTAL_TSC_CHECKS: "false",
        WRANGLER_LOG_PATH: path.join(tempDir, "wrangler-logs"), WRANGLER_LOG_SANITIZE: "true" } }
    );
    const health = await waitFor("local Edge Worker readiness", async () => {
      if (worker.spawnError) throw new Error(worker.outputSummary());
      if (worker.exitCode !== null) throw new Error(worker.outputSummary());
      const result = await fetchJson(`${edgeOrigin}/health`);
      return result.response.ok ? result.body : undefined;
    }, 30_000);
    assert.deepEqual(health, { ok: true, auth: "scaffold", environment: "local" });
  };
  const restartEdge = async (duringOutage) => {
    await crashChild(worker);
    await duringOutage();
    const startedAt = Date.now();
    await startEdge();
    return startedAt;
  };
  await startEdge();
  await ordinaryDeviceSmoke(edgeOrigin, fake.origin, restartEdge);
  const ipcPort = await reservePort();
  const roomProbe = await ownerFetch(
    edgeOrigin,
    `/stats/${SESSION_ID}?deploymentId=${encodeURIComponent(DEPLOYMENT_ID)}`
  );
  assert.equal(
    roomProbe.response.status,
    404,
    `owner session scope probe failed: ${JSON.stringify(roomProbe.body)}`
  );
  const ownerRoomURL =
    `${edgeOrigin.replace("http:", "ws:")}/session/${SESSION_ID}/ws?device=owner-ui&token=${OWNER_TOKEN}&deploymentId=${encodeURIComponent(DEPLOYMENT_ID)}`;

  const ownerSession = await openWebSocket(
    ownerRoomURL,
    "owner session room"
  );
  console.log("PASS Edge authenticated the verified owner and routed its real session WebSocket");

  const grantResult = await ownerFetch(edgeOrigin, "/auth/device-grants", {
    method: "POST",
    body: JSON.stringify({
      deploymentId: DEPLOYMENT_ID,
      sandboxId: SANDBOX_ID,
      targetDeviceId: DEVICE_ID,
      sessionId: SESSION_ID,
      lifecycleEpoch: LIFECYCLE_EPOCH,
      capabilities: ["session.read", "session.control", "session.environment", "session.files"],
      ttlSeconds: 60
    })
  });
  assert.equal(grantResult.response.status, 200, JSON.stringify(grantResult.body));
  assert.equal(typeof grantResult.body?.grant, "string");
  assert.match(grantResult.body.grant, /^cg1\.[a-f0-9]{32}\.[a-f0-9]{64}$/);
  assert.equal(fake.observations.targetProofs, 1, "Edge must verify the exact target with Scaffold");
  const grantId = grantResult.body.grant.split(".")[1];
  console.log("PASS Edge created a real owner-bound device grant after local target proof");

  const bootstrapPath = path.join(tempDir, "device-bootstrap.json");
  const dataDir = path.join(tempDir, "comet-data");
  await writeFile(
    bootstrapPath,
    JSON.stringify({
      deviceJoinGrant: grantResult.body.grant,
      projectId: PROJECT_ID,
      deploymentId: DEPLOYMENT_ID,
      sessionId: SESSION_ID,
      deviceId: DEVICE_ID,
      sandboxId: SANDBOX_ID,
      lifecycleEpoch: LIFECYCLE_EPOCH
    }),
    { mode: 0o600 }
  );
  await chmod(bootstrapPath, 0o600);

  const host = spawnTracked(
    "Rust comet headless host",
    COMET_BIN,
    ["headless", "--device-bootstrap-file", bootstrapPath, "--edge-url", edgeOrigin],
    {
      cwd: ROOT,
      env: {
        ...process.env,
        COMET_DATA_DIR: dataDir,
        COMET_IPC_PORT: String(ipcPort),
        COMET_PROJECT_SCOPE: PROJECT_ID,
        RUST_LOG: "info"
      }
    }
  );

  await waitFor("Rust host IPC readiness", async () => {
    if (host.spawnError) throw new Error(host.outputSummary());
    if (host.exitCode !== null) throw new Error(host.outputSummary());
    return new Promise((resolve) => {
      const socket = net.createConnection({ host: "127.0.0.1", port: ipcPort });
      socket.once("connect", () => {
        socket.destroy();
        resolve(true);
      });
      socket.once("error", () => resolve(false));
    });
  });
  await waitFor("Rust host relay registration", async () => {
    const result = await ownerFetch(edgeOrigin, `/device/${DEVICE_ID}/status`);
    return result.response.ok && result.body?.hostConnected === true;
  });
  console.log("PASS Rust comet headless exchanged the grant, bootstrapped, and registered its host relay");

  const clientA = await openWebSocket(
    `${edgeOrigin.replace("http:", "ws:")}/device/${DEVICE_ID}/ws?role=client&connId=client-a&token=${CLIENT_A_TOKEN}`,
    "authenticated relay client A"
  );
  const clientB = await openWebSocket(
    `${edgeOrigin.replace("http:", "ws:")}/device/${DEVICE_ID}/ws?role=client&connId=client-b&token=${CLIENT_B_TOKEN}`,
    "authenticated relay client B"
  );
  // Exercise the mobile upload contract against the real scoped host, not a
  // permissive local-controller relay. Verify every committed byte on disk.
  const imageBytes = Buffer.alloc(100_019);
  for (let index = 0; index < imageBytes.length; index++) imageBytes[index] = index % 251;
  const uploadId = crypto.randomUUID();
  let uploadRPC = 40;
  for (let offset = 0, seq = 0; offset < imageBytes.length; offset += 45_000, seq++) {
    const reply = await rpcCall(clientA, uploadRPC++, "UploadChunk", {
      uploadId, seq, sessionId: SESSION_ID,
      data: imageBytes.subarray(offset, offset + 45_000).toString("base64")
    });
    assert.equal(reply.ok, true);
  }
  const committed = await rpcCall(clientA, uploadRPC++, "UploadCommit", {
    uploadId, sessionId: SESSION_ID, fileName: "mobile-image.png"
  });
  assert.ok(committed.path.startsWith(dataDir + path.sep));
  assert.deepEqual(await readFile(committed.path), imageBytes);
  await assert.rejects(rpcCall(clientA, uploadRPC++, "UploadChunk", {
    uploadId, sessionId: "other-session", data: "AA=="
  }), { message: "session_scope_denied" });
  console.log("PASS mobile chunk upload committed 100019 byte-identical bytes on scoped Rust host; cross-session upload denied");

  await assert.rejects(rpcCall(clientA, 0, "QueueCommand", {
    command: {
      kind: "control",
      sessionId: SESSION_ID,
      actorSubject: OWNER_SUBJECT,
      action: { action: "pause" }
    }
  }), { message: "actor_mismatch" });
  console.log("PASS Edge rejected a forged command actor from a different authenticated principal");
  const pauseCommand = {
    chatId: SESSION_ID,
    command: {
      kind: "control",
      sessionId: SESSION_ID,
      ownerDeviceId: DEVICE_ID,
      actorDeviceId: "client-b",
      actorSubject: CLIENT_B_SUBJECT,
      grantId,
      source: "scaffold",
      action: { action: "pause" }
    }
  };
  const pauseResult = await rpcCall(clientB, 1, "QueueCommand", pauseCommand);
  assert.ok(pauseResult && typeof pauseResult === "object", "Rust must answer an actual exact-session relay RPC");
  console.log("PASS two authenticated client WebSockets attached and relayed an actual exact-session RPC through Rust");

  await closeWebSocket(clientA, "reconnect client A");
  const reconnectedA = await openWebSocket(
    `${edgeOrigin.replace("http:", "ws:")}/device/${DEVICE_ID}/ws?role=client&connId=client-a&token=${CLIENT_A_TOKEN}`,
    "reconnected relay client A"
  );
  const reconnectPause = {
    chatId: SESSION_ID,
    command: {
      kind: "control",
      sessionId: SESSION_ID,
      ownerDeviceId: DEVICE_ID,
      actorDeviceId: "client-a",
      actorSubject: CLIENT_A_SUBJECT,
      grantId,
      source: "scaffold",
      action: { action: "pause" }
    }
  };
  const reconnectedResult = await rpcCall(reconnectedA, 2, "QueueCommand", reconnectPause);
  assert.ok(reconnectedResult && typeof reconnectedResult === "object");
  console.log("PASS a disconnected client reconnected and relayed again through the same Rust host");

  const revoked = await ownerFetch(edgeOrigin, `/auth/device-grants?id=${grantId}`, {
    method: "DELETE"
  });
  assert.equal(revoked.response.status, 200, JSON.stringify(revoked.body));
  assert.deepEqual(revoked.body, { ok: true });

  await waitFor("revoked host active disconnect", async () => {
    const result = await ownerFetch(edgeOrigin, `/device/${DEVICE_ID}/status`);
    return result.response.ok && result.body?.hostConnected === false;
  });
  const relayDenial = await expectRelayDenial(clientB, 3, "QueueCommand", pauseCommand);
  assert.ok(
    ["host_offline", "host_closed", "socket_closed:4403"].includes(relayDenial),
    `unexpected revocation relay result: ${relayDenial}`
  );
  const consumedGrant = await fetchJson(`${edgeOrigin}/auth/device-grants/exchange`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ grant: grantResult.body.grant })
  });
  assert.equal(consumedGrant.response.status, 401, "a revoked grant must remain denied");
  await delay(3_500);
  const stayedOffline = await ownerFetch(edgeOrigin, `/device/${DEVICE_ID}/status`);
  assert.equal(stayedOffline.response.status, 200);
  assert.equal(stayedOffline.body?.hostConnected, false, "Rust host reconnect with revoked authority must fail");
  assert.ok(fake.observations.sessionChecks >= 6, "Worker must authenticate every owner/client request");
  console.log("PASS revocation actively disconnected the host, denied relay traffic, and blocked Rust reconnect");

  await Promise.allSettled([
    closeWebSocket(ownerSession, "owner session"),
    closeWebSocket(clientB, "client B"),
    closeWebSocket(reconnectedA, "reconnected client A")
  ]);
  console.log("PASS real local collaboration integration smoke complete");
};

try {
  await main();
} catch (error) {
  for (const child of trackedChildren) {
    if (child.outputSummary) console.error(child.outputSummary());
  }
  if (tempDir) {
    const logs = path.join(tempDir, "wrangler-logs");
    for (const file of await readdir(logs).catch(() => [])) {
      if (file.endsWith(".log")) console.error(`Wrangler diagnostic ${file}:\n${await readFile(path.join(logs, file), "utf8")}`);
    }
  }
  throw error;
} finally {
  await cleanup();
}
