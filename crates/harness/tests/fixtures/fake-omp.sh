#!/bin/sh
set -eu
: "${OMP_ARGV_LOG:?}"
printf '%s\n' "$@" > "$OMP_ARGV_LOG"
[ "${1:-}" = "acp" ] || exit 91
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"sessionCapabilities\":{\"resume\":{}}}}}"
      ;;
    session/new)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"omp-session-1\"}}"
      ;;
    session/resume)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}"
      ;;
    session/prompt)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello from omp"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","title":"Read file","rawInput":{"path":"README.md"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-1","status":"completed"}}}'
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"stopReason\":\"end_turn\"}}"
      exit 0
      ;;
    session/cancel)
      exit 0
      ;;
  esac
done
