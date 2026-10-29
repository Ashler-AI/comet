#!/bin/sh
# Reject any fallback to provider credentials or the device's live Google home.
set -eu
[ "${AGY_ACP_ENABLE_GATEWAY_AUTH:-}" = 1 ]
[ "${AGY_ACP_FORCE_FILE_STORAGE:-}" = 1 ]
[ -z "${GEMINI_API_KEY:-}${GOOGLE_API_KEY:-}${GOOGLE_APPLICATION_CREDENTIALS:-}" ]
case "$GEMINI_HOME" in *zeron-antigravity-managed-*/gemini) ;; *) exit 10 ;; esac
[ ! -f "$GEMINI_HOME/antigravity-acp/acp_token.json" ]
authenticated=0
while read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      case "$line" in *'"gateway":true'*) ;; *) exit 11 ;; esac
      printf '{"id":%s,"result":{"protocolVersion":1,"authMethods":[{"id":"gateway"}]}}\n' "$id" ;;
    *'"method":"authenticate"'*)
      case "$line" in *'"Authorization":"Bearer opaque-managed-test"'*) ;; *) exit 12 ;; esac
      case "$line" in *'"baseUrl":"http://127.0.0.1:9876"'*) ;; *) exit 13 ;; esac
      case "$line" in *'"methodId":"gateway"'*) ;; *) exit 14 ;; esac
      authenticated=1
      printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/new"'*)
      [ "$authenticated" = 1 ]
      printf '{"id":%s,"result":{"sessionId":"gateway-1","configOptions":[{"id":"model","category":"model","type":"select","currentValue":"gemini-3.8-flash-high","options":[{"value":"gemini-3.8-flash-high","name":"Gemini Flash"}]}]}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      printf '{"method":"session/update","params":{"sessionId":"gateway-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"gateway-authorized"}}}}\n'
      printf '{"id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      exit 0 ;;
    *) printf '{"id":%s,"result":{}}\n' "$id" ;;
  esac
done
