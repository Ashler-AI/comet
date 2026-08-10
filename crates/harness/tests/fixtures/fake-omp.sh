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
[ "${1:-}" = "--mode" ] && [ "${2:-}" = "rpc" ] || exit 91

SESSION_ID="omp-session-1"
prev=""
for arg in "$@"; do
  if [ "$prev" = "--resume" ]; then
    SESSION_ID="$arg"
  fi
  prev="$arg"
done
ACTIVE_GOAL="${OMP_ACTIVE_GOAL:-}"

printf '%s\n' '{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}'
if [ -n "$ACTIVE_GOAL" ]; then
  printf '%s\n' '{"type":"available_commands_update","commands":[{"name":"goal","description":"Toggle goal mode","input":{"hint":"[objective]"},"subcommands":[{"name":"set","description":"Set or replace the goal","usage":"<objective>"},{"name":"drop","description":"Drop the current goal"}]},{"name":"ralplan","description":"Plan with consensus","input":{"hint":"goal"}},{"name":"security","description":"Run security review"}]}'
else
  printf '%s\n' '{"type":"available_commands_update","commands":[{"name":"ralplan","description":"Plan with consensus","input":{"hint":"goal"}},{"name":"security","description":"Run security review"}]}'
fi

finish_turn() {
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":6,"output":4,"cacheRead":100,"cacheWrite":0,"totalTokens":110}}}'
  printf '%s\n' '{"type":"agent_end","isTerminal":true,"messages":[]}'
}

turn_open=0
while IFS= read -r line; do
  type=$(printf '%s' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$type" in
    negotiate_protocol)
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"negotiate_protocol\",\"success\":true}"
      ;;
    get_state)
      [ -z "${OMP_SESSION_LOG:-}" ] || printf '%s\n' "get_state" >> "$OMP_SESSION_LOG"
      if [ -n "$ACTIVE_GOAL" ]; then
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionId\":\"$SESSION_ID\",\"model\":{\"provider\":\"openai-codex\",\"id\":\"gpt-5.6-sol\"},\"todoPhases\":[],\"goalMode\":{\"enabled\":true,\"mode\":\"active\",\"goal\":{\"id\":\"goal-1\",\"objective\":\"$ACTIVE_GOAL\",\"status\":\"active\"}}}}"
      else
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionId\":\"$SESSION_ID\",\"model\":{\"provider\":\"openai-codex\",\"id\":\"gpt-5.6-sol\"},\"todoPhases\":[]}}"
      fi
      ;;
    prompt)
      [ -z "${OMP_PROMPT_LOG:-}" ] || printf '%s\n' "$line" >> "$OMP_PROMPT_LOG"
      if [ -n "${OMP_PROMPT_ERROR_DETAILS:-}" ]; then
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":false,\"error\":\"$OMP_PROMPT_ERROR_DETAILS\"}"
        continue
      fi
      if has "$line" '/goal set'; then
        ACTIVE_GOAL="Persistent editor indicator"
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":false}}"
        if [ -z "${OMP_OMIT_GOAL_EVENTS:-}" ]; then
          printf '%s\n' '{"type":"goal_updated","goal":{"id":"goal-1","objective":"Persistent editor indicator","status":"active"}}'
        fi
        continue
      fi
      if has "$line" '/goal drop'; then
        ACTIVE_GOAL=""
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":false}}"
        [ -n "${OMP_OMIT_GOAL_EVENTS:-}" ] || printf '%s\n' '{"type":"goal_updated","goal":null}'
        continue
      fi
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":true}}"
      if [ "$turn_open" = "1" ]; then
        # A steer landing in the live turn: the same turn absorbs it and
        # settles with a single agent_end.
        printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"steered reply"}}'
        finish_turn
        turn_open=0
        continue
      fi
      case "$line" in
        *scenario:elicitation*)
          printf '%s\n' '{"type":"extension_ui_request","id":"ui-1","method":"confirm","title":"Provider connection","message":"Allow the provider connection?"}'
          IFS= read -r confirmation || exit 1
          if ! has "$confirmation" '"id":"ui-1"' || ! has "$confirmation" '"confirmed":true'; then
            exit 93
          fi
          printf '%s\n' '{"type":"extension_ui_request","id":"ui-2","method":"select","title":"Region","message":"Choose a provider region.","options":["East","West"]}'
          IFS= read -r selection || exit 1
          if ! has "$selection" '"id":"ui-2"' || ! has "$selection" '"value":"East"'; then
            exit 94
          fi
          ;;
      esac
      printf '%s\n' '{"type":"agent_start"}'
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"thinking"}}'
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello from omp"}}'
      if [ -n "${OMP_STEER_SCENARIO:-}" ]; then
        turn_open=1
        continue
      fi
      printf '%s\n' '{"type":"tool_execution_start","toolCallId":"tool-1","toolName":"read","args":{"path":"README.md"},"intent":"Read file"}'
      printf '%s\n' '{"type":"tool_execution_end","toolCallId":"tool-1","toolName":"read","isError":false,"result":{"content":[{"type":"text","text":"# README"}],"details":{}}}'
      finish_turn
      ;;
    abort)
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"abort\",\"success\":true}"
      ;;
  esac
done
