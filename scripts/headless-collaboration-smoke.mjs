#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const EDGE_DIR = path.join(ROOT, "edge");
const COMET_BIN = process.env.COMET_BIN ?? path.join(ROOT, "target", "debug", "comet");
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
const SESSION_ID = "session-smoke-001";
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
  fetchJson(`${edgeOrigin}${pathname}`, {
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
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      const onOpen = () => {
        dispose();
        resolve(socket);
      };
      const onError = () => {
        dispose();
        reject(new Error(`${label} failed to open`));
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
    label
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

const main = async () => {
  tempDir = await mkdtemp(path.join(os.tmpdir(), "comet-integration-smoke-"));
  const scaffoldPort = await reservePort();
  const fake = await startFakeScaffold(scaffoldPort);
  scaffoldServer = fake.server;
  const edgePort = await reservePort();
  const edgeOrigin = `http://127.0.0.1:${edgePort}`;
  const wrangler = path.join(EDGE_DIR, "node_modules", ".bin", "wrangler");
  const worker = spawnTracked(
    "local Edge Worker",
    wrangler,
    [
      "dev",
      "--local",
      "--ip",
      "127.0.0.1",
      "--port",
      String(edgePort),
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
    { cwd: EDGE_DIR, env: { ...process.env, NO_COLOR: "1" } }
  );

  const health = await waitFor("local Edge Worker readiness", async () => {
    if (worker.spawnError) throw new Error(worker.outputSummary());
    if (worker.exitCode !== null) throw new Error(worker.outputSummary());
    const result = await fetchJson(`${edgeOrigin}/health`);
    return result.response.ok ? result.body : undefined;
  });
  assert.deepEqual(health, { ok: true, auth: "scaffold", environment: "local" });
  const ipcPort = await reservePort();

  const ownerSession = await openWebSocket(
    `${edgeOrigin.replace("http:", "ws:")}/session/${SESSION_ID}/ws?device=owner-ui&token=${OWNER_TOKEN}&deploymentId=${encodeURIComponent(DEPLOYMENT_ID)}`,
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
      capabilities: ["session.read", "session.control", "session.environment"],
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
  const actorDenial = await expectRelayDenial(clientA, 0, "QueueCommand", {
    command: {
      kind: "control",
      sessionId: SESSION_ID,
      actorSubject: OWNER_SUBJECT,
      action: { action: "pause" }
    }
  });
  assert.equal(actorDenial, "actor_mismatch");
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
  throw error;
} finally {
  await cleanup();
}
