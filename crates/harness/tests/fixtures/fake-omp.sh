#!/bin/sh
set -eu
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
if [ "${1:-}" = "-F" ]; then
  case "${OMP_WRITER_STATE:-unknown}" in
    active)
      printf '%s\n' "p4242"
      exit 0
      ;;
    inactive)
      exit 1
      ;;
    *)
      exit 2
      ;;
  esac
fi
: "${OMP_ARGV_LOG:?}"
printf '%s\n' "$@" > "$OMP_ARGV_LOG"
if [ "${1:-}" = "models" ]; then
  printf '%s\n' '{"models":[{"selector":"openai-codex/gpt-5.6-sol","name":"GPT-5.6 Sol","provider":"openai-codex","providerName":"OpenAI Codex","contextWindow":1000000,"maxTokens":262144,"thinking":["low","high","xhigh"]}]}'
  exit 0
fi
[ "${1:-}" = "acp" ] || exit 91
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$method" in
    initialize)
      case "$line" in
        *'"elicitation":{"form":{},"url":{}}'*) ;;
        *) exit 92 ;;
      esac
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true,\"sessionCapabilities\":{\"resume\":{}}}}}"
      ;;
    session/new)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"omp-session-1\",\"configOptions\":[{\"id\":\"model\",\"currentValue\":\"openai-codex/gpt-5.6-sol\"},{\"id\":\"thinking\",\"currentValue\":\"high\"}]}}"
      [ -z "${OMP_SESSION_LOG:-}" ] || printf '%s\n' "session/new" >> "$OMP_SESSION_LOG"
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"ralplan","description":"Plan with consensus","input":{"hint":"goal"}},{"name":"security","description":"Run security review"}]}}}'
      ;;
    session/load|session/resume)
      [ -z "${OMP_METHOD_LOG:-}" ] || printf '%s\n' "$method" >> "$OMP_METHOD_LOG"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}"
      ;;
    session/set_config_option)
      [ -z "${OMP_CONFIG_LOG:-}" ] || printf '%s\n' "$line" >> "$OMP_CONFIG_LOG"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"configOptions\":[]}}"
      ;;
    session/prompt)
      [ -z "${OMP_PROMPT_LOG:-}" ] || printf '%s\n' "$line" >> "$OMP_PROMPT_LOG"
      if [ -n "${OMP_PROMPT_GATE:-}" ]; then
        while [ ! -f "$OMP_PROMPT_GATE" ]; do
          sleep 0.1
        done
      fi
      if [ -n "${OMP_PROMPT_ERROR_DETAILS:-}" ]; then
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32603,\"message\":\"Internal error\",\"data\":{\"details\":\"$OMP_PROMPT_ERROR_DETAILS\"}}}"
        continue
      fi
      case "$line" in
        *scenario:elicitation*)
          printf '%s\n' '{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"toolCall":{"title":"Provider connection"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}}'
          IFS= read -r permission || exit 1
          if ! has "$permission" '"id":900' ||
            ! has "$permission" '"outcome":"selected"' ||
            ! has "$permission" '"optionId":"allow-once"'; then
            printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32603,\"message\":\"provider permission was not accepted\"}}"
            continue
          fi
          printf '%s\n' '{"jsonrpc":"2.0","id":901,"method":"elicitation/create","params":{"message":"Choose a provider region.","mode":"form","requestedSchema":{"type":"object","required":["region"],"properties":{"region":{"type":"string","title":"Region","enum":["East","West"]}}}}}'
          IFS= read -r answer || exit 1
          if ! has "$answer" '"id":901' ||
            ! has "$answer" '"action":"accept"' ||
            ! has "$answer" '"content":{"region":"East"}'; then
            printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32603,\"message\":\"provider elicitation was not accepted\"}}"
            continue
          fi
          ;;
      esac
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello from omp"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","title":"Read file","rawInput":{"path":"README.md"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"omp-session-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-1","status":"completed","rawOutput":{"content":[{"type":"text","text":"# README"}],"details":{}}}}}'
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"stopReason\":\"end_turn\"}}"
      ;;
    session/cancel)
      exit 0
      ;;
  esac
done
