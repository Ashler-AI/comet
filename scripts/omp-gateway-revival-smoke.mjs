#!/usr/bin/env node

// Usage: node scripts/omp-gateway-revival-smoke.mjs [OMP_BINARY] [--compare-explicit]
// No source checkout, credentials, live Crew instance, or external inference is used.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readFile, writeFile, rm, realpath } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
assert(args.every((arg) => !arg.startsWith("--") || arg === "--compare-explicit"), "Unknown option");
assert(args.filter((arg) => !arg.startsWith("--")).length <= 1, "Supply at most one OMP binary");
const BINARY = path.resolve(args.find((arg) => !arg.startsWith("--")) ?? path.join(os.homedir(), ".local/bin/omp"));
const MODEL_ID = "gateway-smoke";
const MODEL = `comet-openai/${MODEL_ID}`;
const TOKEN = "crew-gateway-smoke-not-a-real-credential";
const PROBE = "GatewayProbe";
const TIMEOUT = 60_000;
const children = new Set();
let root;
let server;
let gatewayFailure;
let scenario;
let sequence = 0;
const requests = [];

function timeout(promise, label, milliseconds = TIMEOUT) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`Timed out: ${label}`)), milliseconds); }),
  ]).finally(() => clearTimeout(timer));
}

async function until(label, probe) {
  const deadline = Date.now() + TIMEOUT;
  while (Date.now() < deadline) {
    if (gatewayFailure) throw gatewayFailure;
    const result = await probe();
    if (result) return result;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`Timed out: ${label}`);
}

function insideTemporaryRoot(file) {
  assert(file && !path.relative(root, file).startsWith("..") && !path.isAbsolute(path.relative(root, file)),
    `Session escaped isolated home: ${file}`);
}

// Strict Responses schemas can require nullable optional fields. Build the call
// from the schema actually advertised by this installed binary, not a canned
// transcript or a guessed RPC execute-tool command.
function conform(schema, value, location = "arguments") {
  if (schema === true || !schema) return value;
  if (schema.anyOf || schema.oneOf) {
    const variants = schema.anyOf ?? schema.oneOf;
    for (const variant of variants) {
      try { return conform(variant, value, location); } catch { /* Try the next legitimate schema branch. */ }
    }
    throw new Error(`No matching schema variant for ${location}`);
  }
  if (schema.enum) assert(schema.enum.includes(value), `${location}: invalid enum value`);
  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  if (value === null) {
    assert(types.includes("null") || schema.type === undefined, `${location}: null not allowed`);
    return value;
  }
  if (Array.isArray(value)) {
    assert(types.includes("array") || schema.type === undefined, `${location}: array not allowed`);
    return value.map((item, index) => conform(schema.items, item, `${location}[${index}]`));
  }
  if (typeof value === "object") {
    assert(types.includes("object") || schema.type === undefined, `${location}: object not allowed`);
    const output = {};
    for (const [key, item] of Object.entries(value)) {
      assert(schema.properties?.[key] || schema.additionalProperties !== false, `${location}.${key}: unknown field`);
      output[key] = conform(schema.properties?.[key], item, `${location}.${key}`);
    }
    for (const key of schema.required ?? []) {
      if (Object.hasOwn(output, key)) continue;
      const field = schema.properties?.[key];
      if (field?.default !== undefined) output[key] = field.default;
      else output[key] = conform(field, null, `${location}.${key}`);
    }
    return output;
  }
  assert(types.includes(typeof value) || (typeof value === "number" && types.includes("integer")) || schema.type === undefined,
    `${location}: unexpected ${typeof value}`);
  return value;
}

function toolCall(body, name, values) {
  const tool = body.tools?.find((entry) => entry.type === "function" && entry.name === name);
  assert(tool, `Installed OMP did not advertise ${name}; available: ${body.tools?.map((entry) => entry.name).join(", ")}`);
  if (name === "task" && !tool.parameters?.properties?.tasks) values = values.tasks[0];
  const argumentsJson = JSON.stringify(conform(tool.parameters, values, name));
  const id = `fc_smoke_${++sequence}`;
  return { type: "function_call", id, call_id: `call_${sequence}`, name, arguments: argumentsJson, status: "completed" };
}

function textItem(text) {
  return { type: "message", id: `msg_smoke_${++sequence}`, role: "assistant", status: "completed",
    content: [{ type: "output_text", text, annotations: [] }] };
}

function respondStream(response, item) {
  const envelope = { id: `resp_smoke_${++sequence}`, object: "response", created_at: 1,
    model: MODEL_ID, status: "in_progress", output: [],
    usage: { input_tokens: 20, output_tokens: 10, total_tokens: 30,
      input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } } };
  response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  let eventIndex = 0;
  const event = (type, fields) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, sequence_number: eventIndex++, ...fields })}\n\n`);
  event("response.created", { response: envelope });
  event("response.output_item.added", { output_index: 0,
    item: item.type === "function_call" ? { ...item, arguments: "", status: "in_progress" } : { ...item, content: [], status: "in_progress" } });
  if (item.type === "function_call") {
    event("response.function_call_arguments.delta", { item_id: item.id, output_index: 0, delta: item.arguments });
    event("response.function_call_arguments.done", { item_id: item.id, output_index: 0, arguments: item.arguments });
  } else {
    event("response.content_part.added", { item_id: item.id, output_index: 0, content_index: 0,
      part: { type: "output_text", text: "", annotations: [] } });
    event("response.output_text.delta", { item_id: item.id, output_index: 0, content_index: 0, delta: item.content[0].text });
    event("response.output_text.done", { item_id: item.id, output_index: 0, content_index: 0, text: item.content[0].text });
  }
  event("response.output_item.done", { output_index: 0, item });
  event("response.completed", { response: { ...envelope, status: "completed", output: [item] } });
  response.end();
}

async function gateway(request, response) {
  assert.equal(request.headers.authorization, `Bearer ${TOKEN}`, "Gateway received an unexpected credential");
  const record = { scenario: scenario?.name, phase: scenario?.phase, method: request.method, url: request.url };
  requests.push(record);
  if (request.method === "GET" && request.url === "/v1/models") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ object: "list", data: [{ id: `openai-codex/${MODEL_ID}`,
      owned_by: "openai-codex", api: "openai-codex-responses" }] }));
    return;
  }
  assert.equal(request.method, "POST");
  assert.equal(request.url, "/v1/responses");
  let raw = "";
  for await (const chunk of request) {
    raw += chunk;
    assert(raw.length < 4_000_000, "Unexpectedly large mock request");
  }
  const body = JSON.parse(raw);
  assert.equal(body.model, MODEL_ID);
  assert.equal(body.stream, true);
  assert(scenario, "Inference outside an active scenario");
  record.session = body.prompt_cache_key;
  assert.equal(typeof record.session, "string", "Extension must attach session identity");
  // OMP generates a task UI label off the spawn's critical path. Unlike the
  // agent turn it has no tools and uses the public task handle as cache key.
  // Serve that real auxiliary request separately from the persisted UUID.
  if (record.session === PROBE && !body.tools?.length) {
    record.role = "task-label";
    respondStream(response, textItem("Gateway revival probe"));
    return;
  }
  if (!body.tools?.length && body.instructions?.startsWith("Coding-agent request difficulty classifier:")) {
    record.role = "effort-classifier";
    respondStream(response, textItem("low"));
    return;
  }
  assert(body.tools?.length, `Unexpected tool-free request: ${JSON.stringify(body).slice(0, 12_000)}`);
  if (!scenario.parentId) scenario.parentId = record.session;
  const parent = record.session === scenario.parentId;
  record.role = parent ? "parent" : "child";
  if (!parent) {
    scenario.childCalls++;
    assert.equal(record.session, scenario.childId ??= record.session, "Unexpected extra subagent");
    respondStream(response, toolCall(body, "yield", { data: { smoke: scenario.phase === "revive" ? "CHILD_REVIVED" : "CHILD_CREATED" } }));
    return;
  }
  const turn = scenario.parentCalls++;
  let item;
  if (scenario.phase === "create" && turn === 0) {
    item = toolCall(body, "task", { context: "Isolated deterministic gateway smoke; do not inspect files or run commands.",
      tasks: [{ name: PROBE, agent: "task", task: "Submit the supplied mock result through yield. Skip builds, tests, linters, and formatters." }] });
  } else if (scenario.phase === "revive" && turn === 0) {
    item = toolCall(body, "hub", { op: "list", status: "parked" });
  } else if (scenario.phase === "revive" && turn === 1) {
    const outputs = body.input?.filter((entry) => entry.type === "function_call_output").map((entry) => entry.output).join("\n") ?? "";
    assert(outputs.includes(PROBE), "Parked persisted subagent not present in actual hub list result");
    item = toolCall(body, "hub", { op: "send", to: PROBE, message: "Cold-revival smoke: submit CHILD_REVIVED through yield." });
  } else {
    item = textItem(scenario.phase === "revive" ? "PARENT_AFTER_REVIVAL" : "PARENT_CREATED");
  }
  record.reply = item.type === "function_call" ? item.name : item.content[0].text;
  respondStream(response, item);
}

class Rpc {
  constructor(env, cwd, extra = []) {
    this.frames = [];
    this.pending = new Map();
    this.serial = 0;
    this.stderr = "";
    this.child = spawn(BINARY, ["--mode", "rpc", "--profile", "gateway-smoke", "--cwd", cwd,
      "--no-lsp", "--no-pty", "--no-skills", "--no-rules", "--no-title", "--no-prewalk",
      "--thinking", "off", "--auto-approve", "--tools", "task,hub,yield", ...extra],
    { cwd, env, detached: true, stdio: ["pipe", "pipe", "pipe"] });
    children.add(this.child);
    this.child.stderr.setEncoding("utf8").on("data", (chunk) => { this.stderr = (this.stderr + chunk).slice(-16_000); });
    const fail = (error) => {
      this.failure = error;
      for (const { reject } of this.pending.values()) reject(error);
      this.pending.clear();
    };
    this.child.on("error", fail);
    this.child.stdin.on("error", fail);
    this.child.on("exit", (code, signal) => fail(new Error(`OMP exited (${code ?? signal})\n${this.stderr}`)));
    this.lines = createInterface({ input: this.child.stdout });
    this.lines.on("line", (line) => {
      let frame;
      try { frame = JSON.parse(line); } catch { return; }
      if (frame.type === "rpc_chunk") {
        if (frame.index === 0) this.chunk = { id: frame.chunkId, buffers: [], count: frame.count, bytes: frame.byteLength };
        if (!this.chunk || this.chunk.id !== frame.chunkId || this.chunk.buffers.length !== frame.index) {
          fail(new Error("Invalid RPC chunk sequence")); return;
        }
        this.chunk.buffers.push(Buffer.from(frame.data, "base64"));
        if (this.chunk.buffers.length !== this.chunk.count) return;
        const buffer = Buffer.concat(this.chunk.buffers);
        if (buffer.length !== this.chunk.bytes) { fail(new Error("Invalid RPC chunk length")); return; }
        try { frame = JSON.parse(buffer.toString()); } catch (error) { fail(error); return; }
        this.chunk = undefined;
      }
      this.frames.push(frame);
      if (frame.type === "response" && this.pending.has(frame.id)) {
        const { resolve, reject } = this.pending.get(frame.id);
        this.pending.delete(frame.id);
        if (frame.success) resolve(frame.data);
        else reject(new Error(`RPC ${frame.command}: ${frame.error}`));
      }
    });
  }

  request(type, fields = {}) {
    if (this.failure) return Promise.reject(this.failure);
    const id = `smoke-${++this.serial}`;
    return timeout(new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(`${JSON.stringify({ id, type, ...fields })}\n`);
    }), `RPC ${type}`).finally(() => this.pending.delete(id));
  }

  async ready() {
    const state = await this.request("get_state");
    if (this.frames.some((frame) => frame.type === "rpc_ready" && frame.supportedProtocolVersions?.includes(2))) {
      await this.request("negotiate_protocol", { protocolVersion: 2 });
    }
    await this.request("set_subagent_subscription", { level: "events" });
    if (state.sessionFile) insideTemporaryRoot(state.sessionFile);
    return state;
  }

  async prompt(message, expectedText) {
    const start = this.frames.length;
    await this.request("prompt", { message });
    await until(message, () => {
      if (this.failure) throw this.failure;
      const frames = this.frames.slice(start);
      const error = frames.find((frame) => frame.type === "message_end" && frame.message?.stopReason === "error");
      if (error) throw new Error(error.message.errorMessage ?? "OMP provider request failed");
      const ended = frames.some((frame) => frame.type === "agent_end");
      if (!ended) return false;
      assert(frames.some((frame) => frame.type === "message_end" && frame.message?.content?.some((part) => part.text === expectedText)),
        `Parent did not produce ${expectedText}\n${JSON.stringify(frames.slice(-5))}`);
      for (const frame of frames) {
        if (frame.type === "tool_execution_end") assert(!frame.isError, `Tool failed: ${JSON.stringify(frame)}`);
      }
      return true;
    });
  }

  async stop() {
    await terminate(this.child);
    this.lines.close();
  }
}

async function terminate(child) {
  if (child.exitCode !== null || child.signalCode !== null) { children.delete(child); return; }
  const exited = once(child, "exit");
  try { process.kill(-child.pid, "SIGTERM"); } catch (error) { if (error.code !== "ESRCH") throw error; }
  try { await timeout(exited, "OMP shutdown", 5_000); }
  catch {
    try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    await timeout(exited, "OMP forced shutdown", 5_000);
  }
  children.delete(child);
}

async function prepare(name, { explicit = false, legacy = false } = {}) {
  const home = path.join(root, name);
  const cwd = path.join(home, "workspace");
  const agent = path.join(home, ".omp", "profiles", "gateway-smoke", "agent");
  for (const dir of [cwd, path.join(agent, "extensions"), path.join(agent, "comet-runtime"), path.join(home, "tmp")]) {
    await mkdir(dir, { recursive: true, mode: 0o700 });
  }
  const adapter = path.join(agent, "comet-runtime", "agent-auth-gateway.ts");
  await writeFile(adapter, await readFile(path.join(ROOT, "crates/harness/src/prime_agent_auth_gateway.ts")), { mode: 0o600 });
  if (!explicit) await writeFile(path.join(agent, "extensions", "crew-auth-gateway.ts"),
    await readFile(path.join(ROOT, "crates/harness/src/crew_auth_gateway.ts")), { mode: 0o600 });
  // Simulate an independent user's override without using or changing their file.
  const legacySource = 'import { registerGateway } from "../comet-runtime/agent-auth-gateway.ts";\nexport default async function userGateway(pi) { await registerGateway(pi, "COMET_INFERENCE_TOKEN"); }\n';
  if (legacy) await writeFile(path.join(agent, "extensions", "omp-auth-gateway.ts"), legacySource, { mode: 0o600 });
  await writeFile(path.join(agent, "config.yml"), JSON.stringify({
    autoResume: false,
    modelRoles: { default: MODEL, smol: MODEL, slow: MODEL, plan: MODEL, tiny: MODEL },
    startup: { checkUpdate: false, setupWizard: false }, marketplace: { autoUpdate: "off" },
    memory: { backend: "off" }, memories: { enabled: false }, recap: { enabled: false },
    compaction: { enabled: false }, retry: { enabled: false }, git: { enabled: false },
    task: { batch: true, agentIdleTtlMs: 100, enableLsp: false, agentModelOverrides: { task: MODEL } },
    async: { enabled: false }, tools: { xdev: false, intentTracing: false },
    dev: { autoqa: false }, lsp: { enabled: false },
  }), { mode: 0o600 });
  // Intentionally whitelist, never spread process.env: no inherited provider,
  // broker, MCP, Crew IPC, user profile, proxy, or inference credentials.
  const env = { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: home, USERPROFILE: home,
    TMPDIR: path.join(home, "tmp"), XDG_CONFIG_HOME: path.join(home, ".config"),
    XDG_DATA_HOME: path.join(home, ".local/share"), XDG_STATE_HOME: path.join(home, ".local/state"),
    XDG_CACHE_HOME: path.join(home, ".cache"), PI_CODING_AGENT_DIR: agent,
    OMP_PROFILE: "gateway-smoke", PI_NO_PTY: "1", ASHLER_INCREMENTAL_TSC_CHECKS: "false",
    PRIME_AGENT_AUTH_GATEWAY_URL: `http://127.0.0.1:${server.address().port}`,
    COMET_SESSION_ID: "11111111-1111-4111-8111-111111111111", COMET_INFERENCE_TOKEN: TOKEN,
    OMP_AUTH_GATEWAY_TOKEN: TOKEN, NO_COLOR: "1" };
  return { name, cwd, agent, env, legacySource, extra: explicit ? ["--extension", adapter] : [] };
}

async function checkGuard(name, remove) {
  scenario = { name, phase: "guard" };
  const fixture = await prepare(name);
  for (const key of remove) delete fixture.env[key];
  // RPC startup requires an available model even though this guard never sends
  // a prompt. A fake independent key makes a bundled model selectable; pin its
  // endpoint to the mock too so an accidental call cannot leave loopback.
  fixture.env.OPENAI_API_KEY = TOKEN;
  fixture.env.OPENAI_BASE_URL = `${fixture.env.PRIME_AGENT_AUTH_GATEWAY_URL}/v1`;
  const before = requests.length;
  const rpc = new Rpc(fixture.env, fixture.cwd, ["--model", "openai/gpt-4o"]);
  try {
    await rpc.ready();
    const { models } = await rpc.request("get_available_models");
    assert(!models.some((model) => model.provider === "comet-openai"), "Bare OMP registered Crew provider");
    assert.equal(requests.length, before, "Bare OMP fetched the Crew gateway catalog");
    console.log(`PASS ${name}: no catalog fetch, no Crew provider`);
  } finally { await rpc.stop(); }
}

async function checkLegacy() {
  scenario = { name: "legacy-override", phase: "guard" };
  const fixture = await prepare(scenario.name, { legacy: true });
  const before = requests.length;
  const rpc = new Rpc(fixture.env, fixture.cwd, ["--model", MODEL]);
  try {
    await rpc.ready();
    const { models } = await rpc.request("get_available_models");
    assert.equal(models.filter((model) => model.provider === "comet-openai" && model.id === MODEL_ID).length, 1);
    assert.equal(requests.slice(before).filter((request) => request.url === "/v1/models").length, 1,
      "Managed wrapper duplicated the user override's provider discovery");
    assert.equal(await readFile(path.join(fixture.agent, "extensions/omp-auth-gateway.ts"), "utf8"), fixture.legacySource);
    console.log("PASS legacy override: one catalog fetch, one model, override unchanged");
  } finally { await rpc.stop(); }
}

async function revival(explicit = false) {
  scenario = { name: explicit ? "old-explicit-only" : "managed-discovery", phase: "create", parentCalls: 0, childCalls: 0 };
  const fixture = await prepare(scenario.name, { explicit });
  let rpc = new Rpc(fixture.env, fixture.cwd, ["--model", MODEL, ...fixture.extra]);
  let parent;
  let child;
  try {
    parent = await rpc.ready();
    await rpc.prompt("Create the persisted gateway smoke subagent.", "PARENT_CREATED");
    child = rpc.frames.find((frame) => frame.type === "subagent_lifecycle" && frame.payload.id === PROBE && frame.payload.status === "completed")?.payload;
    assert(child?.sessionFile, "No completed persisted subagent lifecycle frame");
    insideTemporaryRoot(child.sessionFile);
    const transcript = await readFile(child.sessionFile, "utf8");
    assert(transcript.split("\n").some((line) => line && JSON.parse(line).type === "session_init"), "Missing persisted revival contract");
    assert(transcript.includes("CHILD_CREATED"), "Persisted child never yielded the mock result");
    parent = await rpc.request("get_state");
    insideTemporaryRoot(parent.sessionFile);
    assert.equal(scenario.childCalls, 1, "Spawn did not complete in one mock inference call");
  } finally { await rpc.stop(); }

  scenario.phase = "revive";
  scenario.parentCalls = 0;
  const before = requests.length;
  rpc = new Rpc(fixture.env, fixture.cwd, ["--resume", parent.sessionFile, "--model", MODEL, ...fixture.extra]);
  try {
    const resumed = await rpc.ready();
    assert.equal(resumed.sessionId, parent.sessionId, "Parent restart did not resume the original session");
    assert(!rpc.frames.some((frame) => frame.type === "subagent_lifecycle"), "Subagent was already live before cold send");
    let failure;
    try { await rpc.prompt("List parked agents, revive GatewayProbe with hub send, then make another parent provider call.", "PARENT_AFTER_REVIVAL"); }
    catch (error) { failure = error; }
    if (explicit) {
      const after = requests.slice(before);
      assert(failure, "Old explicit-only control unexpectedly survived; regression comparison no longer applies");
      assert(/(?:API key|auth|provider).*comet-openai|comet-openai.*(?:API key|auth|provider)/i.test(failure.message),
        `Old control failed for an unrelated reason: ${failure.message}`);
      assert(rpc.frames.some((frame) => frame.type === "tool_execution_end" && frame.toolName === "hub" &&
        frame.result?.details?.op === "send" && frame.result.details.receipts?.some((receipt) =>
          receipt.to === PROBE && receipt.outcome === "revived")),
      "Old control did not complete an actual cold-revival hub send");
      assert(!after.some((request) => request.reply === "PARENT_AFTER_REVIVAL"), "Old control reached post-revival inference");
      console.log(`PASS negative control: explicit-only installation lost provider auth during cold revival (${failure.message})`);
      return;
    }
    if (failure) throw failure;
    await until("revived child completion", () => {
      if (rpc.failure) throw rpc.failure;
      return rpc.frames.some((frame) => frame.type === "subagent_lifecycle" && frame.payload.id === PROBE && frame.payload.status === "completed");
    });
    const after = requests.slice(before);
    assert.equal(scenario.childCalls, 2, "Cold child did not issue exactly one further inference request");
    assert(after.some((request) => request.role === "child"), "No revived child request reached gateway");
    assert(after.some((request) => request.reply === "PARENT_AFTER_REVIVAL"), "No post-revival parent request reached gateway");
    assert((await readFile(child.sessionFile, "utf8")).includes("CHILD_REVIVED"), "Revived child result not persisted");
    const sends = rpc.frames.filter((frame) => frame.type === "tool_execution_end" && frame.toolName === "hub");
    assert(sends.some((frame) => frame.result?.details?.op === "send" &&
      frame.result.details.receipts?.some((receipt) => receipt.to === PROBE && receipt.outcome === "revived")),
    `No cold-revival hub send receipt: ${JSON.stringify(sends, null, 2)}`);
    console.log(`PASS managed discovery: persisted ${PROBE}, restarted parent ${parent.sessionId}, cold hub revival, child yield, parent inference; ${JSON.stringify({
      createChildRequests: 1, revivedChildRequests: scenario.childCalls - 1,
      revivalParentRequests: after.filter((request) => request.role === "parent").length,
      revivalCatalogRequests: after.filter((request) => request.url === "/v1/models").length,
    })}`);
  } finally { await rpc.stop(); }
}

let cleanupPromise;
function cleanup() {
  return cleanupPromise ??= (async () => {
    await Promise.all([...children].map(terminate));
    if (server) {
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    }
    if (root) await rm(root, { recursive: true, force: true });
  })();
}
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => { cleanup().finally(() => process.exit(signal === "SIGINT" ? 130 : 143)); });
}

try {
  root = await realpath(await mkdtemp(path.join(os.tmpdir(), "crew-omp-gateway-smoke-")));
  server = http.createServer((request, response) => {
    gateway(request, response).catch((error) => {
      gatewayFailure = error;
      if (!response.headersSent) response.writeHead(500, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: { message: error.message } }));
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  await checkGuard("bare-no-crew-env", ["COMET_SESSION_ID", "COMET_INFERENCE_TOKEN"]);
  await checkGuard("bare-token-only", ["COMET_SESSION_ID"]);
  await checkGuard("bare-session-only", ["COMET_INFERENCE_TOKEN"]);
  await checkLegacy();
  await revival();
  if (args.includes("--compare-explicit")) await revival(true);
  if (gatewayFailure) throw gatewayFailure;
  console.log(`PASS installed OMP gateway smoke: ${BINARY}; all requests used deterministic loopback credentials and isolated temporary profiles`);
} catch (error) {
  console.error(`FAIL installed OMP gateway smoke: ${error.stack ?? error}`);
  console.error(`Gateway request trace: ${JSON.stringify(requests)}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}
