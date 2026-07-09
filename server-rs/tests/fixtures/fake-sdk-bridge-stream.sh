#!/usr/bin/env bash
# Streaming fake SDK bridge. Stays alive and reads NDJSON lines from stdin (the same pipe the engine
# injects follow-up turns over). For each USER turn line it emits a turn (init -> text delta -> result).
# Control lines from the engine's SdkHandle are handled like the real bridge:
#   {"__bridge":"end"}       -> end of input, exit cleanly (mirrors stdin EOF)
#   {"__bridge":"interrupt"} -> no turn; stay alive (the real bridge relays query.interrupt())
# Writes to $SDK_BRIDGE_LOG (SdkRunner sends the bridge's stdout to /dev/null).
# Honors FAKE_CLAUDE_SLEEP to delay before each result.
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"fake-stream-1","model":"claude-opus-4-8"}'
n=0
while IFS= read -r line; do
  case "$line" in
    *__bridge*end*) exit 0 ;;
    *__bridge*) continue ;;
  esac
  n=$((n + 1))
  emit '{"type":"system","subtype":"init","session_id":"fake-stream-1"}'
  emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"reply '"$n"'"}}}'
  sleep "${FAKE_CLAUDE_SLEEP:-0}"
  emit '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
done
