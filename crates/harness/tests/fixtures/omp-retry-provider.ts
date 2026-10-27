import { appendFileSync } from "node:fs";

// Runs only in the opt-in native OMP smoke, inside its disposable HOME/cwd.
export default function (pi) {
  let requests = 0;
  const encode = (event) => `data: ${JSON.stringify(event)}\n\n`;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      if (request.method !== "POST" || new URL(request.url).pathname !== "/v1/responses") {
        return new Response("Unexpected fixture request", { status: 404 });
      }
      const body = await request.json();
      appendFileSync("retry-requests.jsonl", JSON.stringify(body) + "\n");
      requests++;
      const id = `resp_fixture_${requests}`;
      const created = { type: "response.created", response: { id, status: "in_progress" } };
      if (requests === 2) {
        // An empty idle stall aborts the provider attempt's request signal.
        // Recovery therefore needs native turn retry, not the HTTP retry loop.
        const stream = new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode(encode(created)));
          },
        });
        return new Response(stream, { headers: { "content-type": "text/event-stream" } });
      }
      if (requests > 3) return new Response("Fixture retry budget exceeded", { status: 400 });
      const events = [created];
      if (requests === 1) {
        const tool = body.tools.find((tool) => tool.name === "read");
        if (!tool) return new Response("Native read tool missing", { status: 400 });
        const item = {
          type: "function_call", id: "fc_before_retry", call_id: "call_before_retry",
          name: tool.name, arguments: JSON.stringify({ i: "Reading completed fixture", path: "completed.txt" }),
        };
        events.push(
          { type: "response.output_item.added", output_index: 0, item: { ...item, arguments: "" } },
          { type: "response.function_call_arguments.delta", output_index: 0, item_id: item.id, delta: item.arguments },
          { type: "response.function_call_arguments.done", output_index: 0, item_id: item.id, arguments: item.arguments },
          { type: "response.output_item.done", output_index: 0, item },
        );
      } else {
        const item = { type: "message", id: "msg_recovered", role: "assistant", status: "completed", content: [{ type: "output_text", text: "RECOVERED", annotations: [] }] };
        events.push(
          { type: "response.output_item.added", output_index: 0, item: { ...item, status: "in_progress", content: [] } },
          { type: "response.content_part.added", output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [] } },
          { type: "response.output_text.delta", output_index: 0, content_index: 0, delta: "RECOVERED" },
          { type: "response.output_item.done", output_index: 0, item },
        );
      }
      events.push({ type: "response.completed", response: { id, status: "completed", usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } } });
      return new Response(events.map(encode).join(""), { headers: { "content-type": "text/event-stream" } });
    },
  });
  pi.registerProvider("crew-retry-fixture", {
    baseUrl: `http://127.0.0.1:${server.port}/v1`,
    apiKey: "local-fixture-key",
    api: "openai-responses",
    models: [{
      id: "fixture", name: "Local retry fixture", reasoning: false, input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 128000, maxTokens: 4096,
    }],
  });
  pi.on("before_provider_request", (_event, ctx) => {
    if (ctx.model?.provider !== "crew-retry-fixture") throw new Error("Smoke forbids non-fixture inference");
  });
  pi.on("session_shutdown", () => server.stop(true));
}
