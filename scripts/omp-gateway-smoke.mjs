#!/usr/bin/env node
// node scripts/omp-gateway-smoke.mjs [OMP_BINARY] [GATEWAY_EXTENSION]
// Uses the installed binary, real extension and task executor; inference is loopback-only.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readFile, writeFile, rm, realpath } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

assert(process.argv.length <= 4, "Usage: omp-gateway-smoke.mjs [OMP_BINARY] [GATEWAY_EXTENSION]");
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.resolve(process.argv[2] ?? path.join(os.homedir(), ".local/bin/omp"));
const extension = path.resolve(process.argv[3] ?? path.join(repo, "crates/harness/src/omp/auth_gateway.ts"));
const modelId = "gpt-6.1-sol";
const model = "comet-openai/" + modelId;
const probe = "GatewayScout";
const token = "crew-smoke-not-a-real-credential";
const marker = "GATEWAY_SCOUT_COMPLETED";
const frames = [], requests = [], pending = new Map();
let root, server, child, lines, failure, stderr = "", sequence = 0, parentId, childId;
let parentCalls = 0, childCalls = 0;

function timeout(promise, label, ms = 60000) {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("Timed out: " + label + "\n" + stderr)), ms);
  })]).finally(() => clearTimeout(timer));
}
function fail(error) {
  failure ??= error;
  for (const entry of pending.values()) entry.reject(error);
  pending.clear();
}
function textItem(text) {
  return { type: "message", id: "msg_" + ++sequence, role: "assistant", status: "completed",
    content: [{ type: "output_text", text, annotations: [] }] };
}
function respond(response, item) {
  const envelope = { id: "resp_" + ++sequence, object: "response", created_at: 1,
    model: modelId, status: "in_progress", output: [],
    usage: { input_tokens: 20, output_tokens: 10, total_tokens: 30,
      input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } } };
  response.writeHead(200, { "content-type": "text/event-stream" });
  let index = 0;
  const event = (type, fields) => response.write("event: " + type + "\ndata: " +
    JSON.stringify({ type, sequence_number: index++, ...fields }) + "\n\n");
  event("response.created", { response: envelope });
  event("response.output_item.added", { output_index: 0, item: item.type === "function_call"
    ? { ...item, arguments: "", status: "in_progress" } : { ...item, content: [], status: "in_progress" } });
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
  assert.equal(request.headers.authorization, "Bearer " + token);
  if (request.method === "GET" && request.url === "/v1/models") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ data: [{ id: "openai-codex/" + modelId,
      owned_by: "openai-codex", api: "openai-codex-responses" }] }));
    return;
  }
  assert.equal(request.method, "POST");
  assert.equal(request.url, "/v1/responses");
  let raw = "";
  for await (const chunk of request) { raw += chunk; assert(raw.length < 4000000); }
  const body = JSON.parse(raw);
  assert.equal(body.model, modelId, "A role escaped the managed model override");
  assert.equal(body.stream, true);
  const session = body.prompt_cache_key;
  assert.equal(typeof session, "string", "Gateway hook must attach session identity");
  requests.push({ session, model: body.model });
  // These auxiliary requests are real runtime behavior, not agent turns.
  if (!body.tools?.length && session === probe) return respond(response, textItem("Gateway scout"));
  if (!body.tools?.length && body.instructions?.startsWith("Coding-agent request difficulty classifier:")) {
    return respond(response, textItem("low"));
  }
  parentId ??= session;
  if (session !== parentId) {
    childId ??= session;
    assert.equal(session, childId, "Unexpected additional child identity");
    assert.equal(++childCalls, 1, "Scout should complete in one request");
    return respond(response, textItem(marker));
  }
  if (++parentCalls === 1) {
    const task = body.tools?.find(tool => tool.type === "function" && tool.name === "task");
    assert(task, "Installed OMP must advertise its real task tool");
    assert.equal(task.strict, false, "Crew must preserve task strict:false (negative baseline fails here)");
    const item = task.parameters?.properties?.tasks?.items;
    assert(item?.properties, "Expected batch task schema");
    for (const key of ["sourceSession", "model"]) {
      assert(item.properties[key], "Expected advertised seance field: " + key);
      assert(!(item.required ?? []).includes(key), key + " must remain optional");
    }
    const args = { context: "Isolated gateway regression; do not run commands or inspect files.",
      tasks: [{ name: probe, agent: "scout", task: "Return the supplied completion text without tools.",
        solutionSpace: "One deterministic read-only completion; skip builds, tests, linters and formatters." }] };
    const id = "fc_" + ++sequence;
    return respond(response, { type: "function_call", id, call_id: id, name: "task",
      arguments: JSON.stringify(args), status: "completed" });
  }
  assert.equal(parentCalls, 2);
  assert.equal(childCalls, 1, "Task did not launch the real scout");
  const output = body.input?.filter(item => item.type === "function_call_output").map(item => item.output).join("\n");
  assert(output?.includes(marker), "Native task result did not return child completion to parent");
  respond(response, textItem("GATEWAY_PARENT_COMPLETED"));
}
function rpc(type, fields = {}) {
  if (failure) return Promise.reject(failure);
  const id = "rpc_" + ++sequence;
  return timeout(new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ id, type, ...fields }) + "\n");
  }), type).finally(() => pending.delete(id));
}
async function cleanupResources() {
  if (child?.pid && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit");
    try { process.kill(-child.pid, "SIGTERM"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    try { await timeout(exited, "shutdown", 5000); }
    catch { try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
      await timeout(exited, "forced shutdown", 5000); }
  }
  lines?.close();
  if (server) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
  if (root) await rm(root, { recursive: true, force: true });
}
let cleanupPromise;
function cleanup() { return cleanupPromise ??= cleanupResources(); }
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => { cleanup().finally(() => process.exit(signal === "SIGINT" ? 130 : 143)); });
}
try {
  root = await realpath(await mkdtemp(path.join(os.tmpdir(), "crew-gateway-smoke-")));
  const cwd = path.join(root, "workspace"), agent = path.join(root, ".omp/profiles/gateway-smoke/agent");
  await mkdir(cwd, { recursive: true });
  await mkdir(path.join(agent, "extensions"), { recursive: true });
  await mkdir(path.join(root, "tmp"));
  await writeFile(path.join(agent, "extensions/crew-auth-gateway.ts"), await readFile(extension), { mode: 0o600 });
  // Deliberately conflicting saved role: only the launcher's process overrides repair it.
  await writeFile(path.join(agent, "config.yml"), JSON.stringify({
    autoResume: false, modelRoles: { default: model, smol: "openai/gpt-4o-mini", slow: "openai/gpt-4o-mini", plan: "openai/gpt-4o-mini" },
    startup: { checkUpdate: false, setupWizard: false }, marketplace: { autoUpdate: "off" },
    memory: { backend: "off" }, memories: { enabled: false }, recap: { enabled: false },
    compaction: { enabled: false }, retry: { enabled: false }, git: { enabled: false },
    task: { batch: true, structured: false, enableLsp: false }, async: { enabled: false },
    tools: { xdev: false }, dev: { autoqa: false }, lsp: { enabled: false },
  }), { mode: 0o600 });
  server = http.createServer((request, response) => gateway(request, response).catch(error => {
    fail(error);
    if (!response.headersSent) response.writeHead(500, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: { message: error.message } }));
  }));
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  // Never inherit credentials, proxy settings, Crew IPC, extensions or a user profile.
  const env = { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: root, USERPROFILE: root,
    TMPDIR: path.join(root, "tmp"), XDG_CONFIG_HOME: path.join(root, ".config"),
    XDG_DATA_HOME: path.join(root, ".local/share"), XDG_STATE_HOME: path.join(root, ".local/state"),
    XDG_CACHE_HOME: path.join(root, ".cache"), PI_CODING_AGENT_DIR: agent, OMP_PROFILE: "gateway-smoke",
    OMP_AUTH_GATEWAY_URL: "http://127.0.0.1:" + server.address().port, OMP_AUTH_GATEWAY_TOKEN: token,
    PI_SMOL_MODEL: model, PI_SLOW_MODEL: model, PI_PLAN_MODEL: model,
    PI_NO_PTY: "1", NO_COLOR: "1", ASHLER_INCREMENTAL_TSC_CHECKS: "false" };
  child = spawn(binary, ["--mode", "rpc", "--profile", "gateway-smoke", "--cwd", cwd,
    "--model", model, "--no-lsp", "--no-pty", "--no-title", "--no-prewalk", "--thinking", "off", "--tools", "task"],
    { cwd, env, detached: true, stdio: ["pipe", "pipe", "pipe"] });
  child.stderr.setEncoding("utf8").on("data", chunk => { stderr = (stderr + chunk).slice(-16000); });
  child.on("error", fail);
  child.stdin.on("error", fail);
  child.on("exit", (code, signal) => fail(new Error("OMP exited: " + (code ?? signal) + "\n" + stderr)));
  lines = createInterface({ input: child.stdout });
  let chunk;
  lines.on("line", line => {
    try {
      let frame;
      try { frame = JSON.parse(line); } catch { return; }
      if (frame.type === "rpc_chunk") {
        if (frame.index === 0) chunk = { id: frame.chunkId, buffers: [], count: frame.count, bytes: frame.byteLength };
        assert(chunk && chunk.id === frame.chunkId && chunk.buffers.length === frame.index, "Invalid RPC chunks");
        chunk.buffers.push(Buffer.from(frame.data, "base64"));
        if (chunk.buffers.length !== chunk.count) return;
        const bytes = Buffer.concat(chunk.buffers);
        assert.equal(bytes.length, chunk.bytes);
        frame = JSON.parse(bytes.toString()); chunk = undefined;
      }
      frames.push(frame);
      if (frame.type === "response" && pending.has(frame.id)) {
        const entry = pending.get(frame.id); pending.delete(frame.id);
        if (frame.success) entry.resolve(frame.data); else entry.reject(new Error(JSON.stringify(frame)));
      }
      if (frame.type === "tool_execution_end" && frame.isError) fail(new Error(JSON.stringify(frame)));
      if (frame.type === "message_end" && frame.message?.stopReason === "error") fail(new Error(frame.message.errorMessage));
    } catch (error) { fail(error); }
  });
  await rpc("get_state");
  if (frames.some(frame => frame.type === "ready" && frame.supportedProtocolVersions?.includes(2))) {
    await rpc("negotiate_protocol", { protocolVersion: 2 });
  }
  await rpc("set_subagent_subscription", { level: "events" });
  await rpc("prompt", { message: "Run the isolated read-only scout gateway contract probe." });
  await timeout((async () => {
    while (!frames.some(frame => frame.type === "agent_end")) {
      if (failure) throw failure;
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  })(), "parent completion");
  if (failure) throw failure;
  const result = frames.find(frame => frame.type === "tool_execution_end" && frame.toolName === "task")?.result;
  const scout = result?.details?.results?.find(entry => entry.id === probe);
  assert(scout, "Missing native completed scout result");
  assert.equal(scout.agent, "scout");
  assert.equal(scout.exitCode, 0);
  assert.equal(scout.modelRole, "smol");
  assert((scout.resolvedModelIdentity ?? scout.resolvedModel)?.startsWith(model), "Wrong resolved scout model");
  assert(scout.output.includes(marker));
  assert(childId && childId !== parentId, "Missing independent child inference identity");
  assert.equal(childCalls, 1);
  assert.equal(parentCalls, 2);
  assert(frames.some(frame => frame.type === "message_end" && frame.message?.content?.some(part => part.text === "GATEWAY_PARENT_COMPLETED")));
  console.log("PASS real gateway task strict:false, optional seance fields, scout @smol override and native child result: " + binary);
} catch (error) {
  console.error("FAIL gateway smoke: " + (error.stack ?? error));
  console.error(JSON.stringify({ requests, stderr }));
  process.exitCode = 1;
} finally { await cleanup(); }
