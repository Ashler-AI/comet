#!/bin/sh
set -eu
: "${PRIME_ARGV_LOG:?}"
printf '%s\n' "$@" > "$PRIME_ARGV_LOG"

case " $* " in
  *" model list "*)
    printf '%s\n' 'provider model context max-out thinking images' >&2
    printf '%s\n' 'openai-codex gpt-5.6-sol 1.0M 262.1K yes yes' >&2
    exit 0
    ;;
  *" --mode rpc "*)
    while IFS= read -r line; do
      case "$line" in
        *'"type":"get_commands"'*)
          printf '%s\n' '{"id":"comet-commands","type":"response","command":"get_commands","success":true,"data":{"commands":[{"name":"skill:review","description":"Review changes","source":"skill"},{"name":"session_name","description":"Rename the session","source":"extension"}]}}'
          ;;
      esac
    done
    exit 0
    ;;
esac

session_dir=
resume=
previous=
for arg in "$@"; do
  if [ "$previous" = "session-dir" ]; then
    session_dir=$arg
    previous=
  elif [ "$previous" = "resume" ]; then
    resume=$arg
    previous=
  elif [ "$arg" = "--session-dir" ]; then
    previous=session-dir
  elif [ "$arg" = "--resume" ]; then
    previous=resume
  fi
done

if [ -n "$session_dir" ]; then
  mkdir -p "$session_dir"
  printf '%s\n' '{"type":"session","id":"native-prime-session","cwd":"/repo","rlmDepth":0,"timestamp":"2026-08-06T00:00:00Z"}' > "$session_dir/native-prime-session.jsonl"
fi
if [ -n "${PRIME_RESUME_LOG:-}" ]; then
  printf '%s' "$resume" > "$PRIME_RESUME_LOG"
fi

while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"sessionCapabilities\":{\"resume\":{}}}}}"
      ;;
    session/new)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"prime-acp-session\"}}"
      ;;
    session/prompt)
      if [ -n "${PRIME_LONG_TOOL_GATE:-}" ]; then
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"prime-acp-session","update":{"sessionUpdate":"tool_call","toolCallId":"long-tool","title":"Long operation","status":"in_progress"}}}'
        while [ ! -f "$PRIME_LONG_TOOL_GATE" ]; do
          sleep 0.1
        done
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"prime-acp-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"long-tool","status":"completed"}}}'
      fi
      if [ -n "${PRIME_POST_TOOL_GATE:-}" ]; then
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"prime-acp-session","update":{"sessionUpdate":"tool_call","toolCallId":"completed-tool","title":"Completed operation","status":"in_progress"}}}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"prime-acp-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"completed-tool","status":"completed"}}}'
        while [ ! -f "$PRIME_POST_TOOL_GATE" ]; do
          sleep 0.1
        done
      fi
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"prime-acp-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello from prime"}}}}'
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"stopReason\":\"end_turn\"}}"
      exit 0
      ;;
    session/cancel)
      exit 0
      ;;
  esac
done
