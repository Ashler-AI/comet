#!/bin/sh
# Reject any fallback to provider credentials or the device's live Google home.
set -eu
[ "${AGY_ACP_ENABLE_GATEWAY_AUTH:-}" = 1 ]
[ "${AGY_ACP_FORCE_FILE_STORAGE:-}" = 1 ]
[ -z "${GEMINI_API_KEY:-}${GOOGLE_API_KEY:-}${GOOGLE_APPLICATION_CREDENTIALS:-}" ]
if [ -n "${ZERON_TEST_EXPECTED_GEMINI_HOME:-}" ]; then
  [ "$GEMINI_HOME" = "$ZERON_TEST_EXPECTED_GEMINI_HOME" ] || exit 10
else
  case "$GEMINI_HOME" in *zeron-antigravity-managed-*/antigravity) ;; *) exit 10 ;; esac
fi
[ ! -e "$GEMINI_HOME/antigravity-acp/acp_token.json" ]
authenticated=0
reply="gateway-authorized"
session_id="gateway-1"
while read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      case "$line" in *'"gateway":true'*) ;; *) exit 11 ;; esac
      printf '{"id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}}},"authMethods":[{"id":"gateway"}]}}\n' "$id" ;;
    *'"method":"authenticate"'*)
      case "$line" in *'"Authorization":"Bearer opaque-managed-test"'*) ;; *) exit 12 ;; esac
      case "$line" in *'"baseUrl":"http://127.0.0.1:9876"'*) ;; *) exit 13 ;; esac
      case "$line" in *'"methodId":"gateway"'*) ;; *) exit 14 ;; esac
      authenticated=1
      printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/resume"'*)
      [ "$authenticated" = 1 ]
      sid=$(printf '%s' "$line" | sed 's/.*"sessionId":"\([^"]*\)".*/\1/')
      if [ -f "$GEMINI_HOME/antigravity-acp/conversations/$sid.db" ]; then
        session_id="$sid"
        reply="gateway-resumed"
        printf '{"id":%s,"result":{"configOptions":[{"id":"model","category":"model","type":"select","currentValue":"gemini-3.8-flash-high","options":[{"value":"gemini-3.8-flash-high","name":"Gemini Flash"}]}]}}\n' "$id"
      else
        printf '{"id":%s,"error":{"code":-32002,"message":"Session not found in the current GEMINI_HOME"}}\n' "$id"
      fi ;;
    *'"method":"session/new"'*)
      [ "$authenticated" = 1 ]
      mkdir -p "$GEMINI_HOME/antigravity-acp/conversations"
      printf 'db' > "$GEMINI_HOME/antigravity-acp/conversations/gateway-1.db"
      session_id="gateway-1"
      reply="gateway-authorized"
      printf '{"id":%s,"result":{"sessionId":"gateway-1","configOptions":[{"id":"model","category":"model","type":"select","currentValue":"gemini-3.8-flash-high","options":[{"value":"gemini-3.8-flash-high","name":"Gemini Flash"}]}]}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      printf '{"method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"%s"}}}}\n' "$session_id" "$reply"
      printf '{"id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      exit 0 ;;
    *) printf '{"id":%s,"result":{}}\n' "$id" ;;
  esac
done
