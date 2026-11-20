#!/bin/sh
# Crew: a stand-in for `omp` (`models --json`, and `--mode rpc`) for
# tests/crew_omp.rs. Adapted from Crew's tests/fixtures/fake-omp.sh.
#
# Set through the run's environment: OMP_ARGV_LOG, OMP_ENV_LOG, OMP_RPC_PID_LOG
# and OMP_PROMPT_LOG record what it was given. OMP_STEER_SCENARIO keeps the
# first turn open until a steer or an abort, OMP_HANG_ABORT ignores aborts,
# OMP_TURN_ERROR ends turns on a provider error, OMP_STATE_BURST streams 512
# frames ahead of a later get_state's answer, OMP_OMIT_GOAL_EVENTS sends no
# goal_updated, and OMP_REPORTED_SESSION_ID reports another session.
set -eu
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
[ -z "${OMP_ARGV_LOG:-}" ] || printf '%s\n' "$@" > "$OMP_ARGV_LOG"
[ -z "${OMP_ENV_LOG:-}" ] || env > "$OMP_ENV_LOG"

if [ "${1:-}" = "models" ]; then
  if [ "${2:-}" != "--json" ] || [ "${3:-}" != "--no-extensions" ]; then
    echo "unexpected flags: $*" >&2
    exit 2
  fi
  printf '%s\n' '{"models":[{"provider":"anthropic","kind":"chat","id":"claude-opus-5-5","selector":"anthropic/claude-opus-5-5","name":"Claude Opus 5.5","contextWindow":1000000,"maxTokens":128000,"reasoning":true,"thinking":["low","medium","high","xhigh","max"]},{"provider":"openai","kind":"chat","id":"gpt-6.2-sol","selector":"openai/gpt-6.2-sol","name":"GPT-6.2 Sol","contextWindow":272000,"maxTokens":128000,"reasoning":true,"thinking":["low","medium","high","xhigh","max"]}]}'
  exit 0
fi
[ "${1:-}" = "--mode" ] && [ "${2:-}" = "rpc" ] || exit 91
[ -z "${OMP_RPC_PID_LOG:-}" ] || printf '%s\n' "$$" > "$OMP_RPC_PID_LOG"

SESSION_ID="omp-session-1"
MODEL="anthropic/claude-opus-5-5"
CONFIG_PATH=""
EXTENSION_PATH=""
CATALOG=0
prev=""
for arg in "$@"; do
  case "$prev" in
    --resume) SESSION_ID="$arg" ;;
    --config) [ -f "$arg" ] || exit 92; CONFIG_PATH="$arg" ;;
    --extension) EXTENSION_PATH="$arg" ;;
    --model) MODEL="$arg" ;;
  esac
  [ "$arg" != "--comet-command-catalog" ] || CATALOG=1
  prev="$arg"
done
if [ -f "$SESSION_ID" ]; then
  SESSION_ID=$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$SESSION_ID" | sed -n '1p')
fi
SESSION_ID="${OMP_REPORTED_SESSION_ID:-$SESSION_ID}"
PROVIDER="${MODEL%%/*}"
MODEL_ID="${MODEL#*/}"
ACTIVE_GOAL=""

# Every launch reads its overlay last, and none is left in the environment.
[ -f "$CONFIG_PATH" ] || exit 92
[ "${PI_CONFIG_FILES+x}" != x ] || exit 92
[ "$CATALOG" = 0 ] || [ -f "$EXTENSION_PATH" ] || exit 93

COMMANDS='{"name":"ralplan","description":"Plan with consensus","input":{"hint":"goal"}},{"name":"advisor","description":"Toggle advisor"}'
# Discovery's guard, by its path: the test sees it gone once discovery is over.
[ "$CATALOG" = 0 ] || COMMANDS="$COMMANDS,{\"name\":\"guard\",\"description\":\"$EXTENSION_PATH\"}"

printf '%s\n' '{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}'
printf '{"type":"available_commands_update","commands":[%s]}\n' "$COMMANDS"

finish_turn() {
  if [ -n "${OMP_TURN_ERROR:-}" ]; then
    printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"No API key for provider: comet-anthropic","usage":{"input":6,"output":4,"cacheRead":100,"cacheWrite":0}}}'
  else
    printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":6,"output":4,"cacheRead":100,"cacheWrite":0}}}'
  fi
  printf '%s\n' '{"type":"agent_end","isTerminal":true,"messages":[]}'
}

state() {
  goal=""
  if [ -n "$ACTIVE_GOAL" ]; then
    goal=",\"goalMode\":{\"enabled\":true,\"mode\":\"active\",\"goal\":{\"id\":\"goal-1\",\"objective\":\"$ACTIVE_GOAL\",\"status\":\"active\"}}"
  fi
  printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"sessionId":"%s","model":{"provider":"%s","id":"%s","contextWindow":1000000},"todoPhases":[{"tasks":[{"content":"read","status":"completed"},{"content":"fix","status":"in_progress"}]}]%s}}\n' "$1" "$SESSION_ID" "$PROVIDER" "$MODEL_ID" "$goal"
}

turn_open=0
state_requests=0
while IFS= read -r line; do
  type=$(printf '%s' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$type" in
    negotiate_protocol)
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"negotiate_protocol\",\"success\":true}"
      ;;
    get_available_commands)
      printf '{"type":"response","id":"%s","command":"get_available_commands","success":true,"data":{"commands":[%s]}}\n' "$id" "$COMMANDS"
      ;;
    get_state)
      state_requests=$((state_requests + 1))
      if [ "$state_requests" -gt 1 ] && [ -n "${OMP_STATE_BURST:-}" ]; then
        i=0
        while [ "$i" -lt 512 ]; do
          printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"queued event\n"}}'
          i=$((i + 1))
        done
      fi
      state "$id"
      ;;
    prompt)
      [ -z "${OMP_PROMPT_LOG:-}" ] || printf '%s\n' "$line" >> "$OMP_PROMPT_LOG"
      if has "$line" '/mcp'; then
        printf '%s\n' '{"type":"command_output","text":"MCP server management\n\n/mcp list   List configured servers"}'
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":false}}"
        continue
      fi
      if has "$line" '/goal set'; then
        ACTIVE_GOAL="Persistent editor indicator"
        printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":false}}"
        [ -n "${OMP_OMIT_GOAL_EVENTS:-}" ] || printf '%s\n' '{"type":"goal_updated","goal":{"id":"goal-1","objective":"Persistent editor indicator","status":"active"}}'
        continue
      fi
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"prompt\",\"success\":true,\"data\":{\"agentInvoked\":true}}"
      if [ "$turn_open" = 1 ]; then
        # A steer the live turn takes: one agent_end settles both.
        printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"steered reply"}}'
        finish_turn
        turn_open=0
        continue
      fi
      if has "$line" 'scenario:elicitation'; then
        printf '%s\n' '{"type":"extension_ui_request","id":"ui-1","method":"confirm","title":"Provider connection","message":"Allow the provider connection?"}'
        IFS= read -r confirmation || exit 1
        has "$confirmation" '"id":"ui-1"' && has "$confirmation" '"confirmed":true' || exit 94
        printf '%s\n' '{"type":"extension_ui_request","id":"ui-2","method":"select","title":"Region","message":"Choose a provider region.","options":["East","West"]}'
        IFS= read -r selection || exit 1
        has "$selection" '"id":"ui-2"' && has "$selection" '"value":"East"' || exit 95
      fi
      printf '%s\n' '{"type":"agent_start"}'
      if has "$line" 'scenario:retry'; then
        printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"toolUse"}}'
        printf '%s\n' '{"type":"tool_execution_start","toolCallId":"before-retry","toolName":"read","args":{"path":"completed.txt"}}'
        printf '%s\n' '{"type":"tool_execution_end","toolCallId":"before-retry","toolName":"read","isError":false,"result":{"content":[{"type":"text","text":"already completed"}],"details":{}}}'
        printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"OpenAI responses stream stalled while waiting for the next event","content":[]}}'
        printf '%s\n' '{"type":"auto_retry_start","attempt":1,"maxAttempts":10,"delayMs":0}'
        printf '%s\n' '{"type":"agent_end","isTerminal":false,"messages":[]}'
        printf '%s\n' '{"type":"agent_start"}'
      fi
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"thinking"}}'
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello from omp"}}'
      if [ -n "${OMP_STEER_SCENARIO:-}" ]; then
        turn_open=1
        continue
      fi
      printf '%s\n' '{"type":"tool_execution_start","toolCallId":"tool-1","toolName":"read","args":{"path":"README.md"},"intent":"Read file"}'
      printf '%s\n' '{"type":"tool_execution_end","toolCallId":"tool-1","toolName":"read","isError":false,"result":{"content":[{"type":"text","text":"# README"}],"details":{}}}'
      if has "$line" 'scenario:todo'; then
        printf '%s\n' '{"type":"tool_execution_start","toolCallId":"todo-1","toolName":"todo","args":{"op":"update"}}'
        printf '%s\n' '{"type":"tool_execution_end","toolCallId":"todo-1","toolName":"todo","isError":false,"result":{"content":[{"type":"text","text":"updated"}]}}'
      fi
      finish_turn
      if has "$line" 'scenario:autonomous'; then
        printf '%s\n' '{"type":"agent_start"}'
        printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"autonomous reply"}}'
        finish_turn
      fi
      ;;
    abort)
      [ -z "${OMP_HANG_ABORT:-}" ] || continue
      printf '%s\n' "{\"type\":\"response\",\"id\":\"$id\",\"command\":\"abort\",\"success\":true}"
      if [ "$turn_open" = 1 ]; then
        finish_turn
        turn_open=0
      fi
      ;;
  esac
done
