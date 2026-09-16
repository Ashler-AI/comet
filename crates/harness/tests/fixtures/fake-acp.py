#!/usr/bin/env python3
"""Deterministic ACP peer; receives its scenario from the executable filename."""
import json
import pathlib
import sys
import time

agent = pathlib.Path(sys.argv[0]).stem
session = "acp-session"
model = ""
pending = None


def send(value):
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)


def reply(request, result):
    send(dict(id=request["id"], result=result))


def update(value):
    send(dict(method="session/update", params=dict(sessionId=session, update=value)))


def catalog():
    if agent == "grok":
        return dict(models=dict(availableModels=[dict(modelId="chosen", name="Chosen")]))
    return dict(configOptions=[dict(id="selected_model", category="model", type="select", options=[dict(value="chosen", name="Chosen")])])


if sys.argv[1:] == ["models", "list", "--format", "json"]:
    print(json.dumps(dict(families=[dict(variants=[dict(model_uid="chosen", label="Chosen")])])), flush=True)
    sys.exit(0)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    if method == "initialize":
        reply(request, dict(protocolVersion=1, agentCapabilities=dict(loadSession=True)))
    elif method in ("session/new", "session/load"):
        if method == "session/load" and params["sessionId"] != session:
            send(dict(id=request["id"], error=dict(code=-32602, message="wrong resume id")))
            continue
        if method == "session/load":
            # More than the bounded RPC notification channel: startup must
            # drain replay without duplicating old assistant text.
            for _ in range(300):
                update(dict(sessionUpdate="agent_message_chunk", content=dict(type="text", text="old history")))
        state = dict(sessionId=session, **catalog())
        if agent == "devin":
            state["configOptions"][0]["options"] = [dict(value="stale", name="Stale bundled model")]
        reply(request, state)
        if agent == "devin":
            update(dict(sessionUpdate="config_option_update", **catalog()))
        time.sleep(0.03)
        update(dict(sessionUpdate="available_commands_update", availableCommands=[dict(name="review", description="Review changes", input=dict(hint="path"))]))
    elif method in ("session/set_model", "session/set_config_option"):
        model = params.get("modelId", params.get("value"))
        reply(request, {})
    elif method == "session/prompt":
        if model != "chosen":
            send(dict(id=request["id"], error=dict(code=-32602, message="selected model was not applied")))
            continue
        prompt = params["prompt"][-1]["text"]
        if prompt == "wait":
            update(dict(sessionUpdate="agent_message_chunk", content=dict(type="text", text="waiting")))
            continue
        if prompt == "permission":
            pending = request
            send(dict(id="permission-check", method="session/request_permission", params=dict(sessionId=session, toolCall=dict(title="Sensitive action"), options=[dict(optionId="allow", name="Allow once", kind="allow_once"), dict(optionId="deny", name="Deny", kind="reject_once")])))
            continue
        update(dict(sessionUpdate="agent_message_chunk", content=dict(type="text", text="answer:" + prompt)))
        update(dict(sessionUpdate="tool_call_update", toolCallId="tool", status="completed", content=[dict(type="content", content=dict(type="text", text="tool output"))]))
        if agent == "grok":
            prompt_id = params["_meta"]["promptId"]
            send(dict(method="_x.ai/session/prompt_complete", params=dict(sessionId=session, promptId="stale", stopReason="cancelled")))
            send(dict(method="_x.ai/session/prompt_complete", params=dict(sessionId=session, promptId=prompt_id, stopReason="end_turn")))
            # Deliberately no RPC response: Grok's extension must settle it.
        else:
            reply(request, dict(stopReason="end_turn"))
    elif request.get("id") == "permission-check" and pending:
        outcome = request["result"]["outcome"]
        allowed = outcome.get("optionId") == "allow" and outcome["outcome"] == "selected"
        update(dict(sessionUpdate="agent_message_chunk", content=dict(type="text", text="allowed" if allowed else "denied")))
        reply(pending, dict(stopReason="end_turn"))
        pending = None
    elif method == "session/cancel":
        sys.exit(0)
