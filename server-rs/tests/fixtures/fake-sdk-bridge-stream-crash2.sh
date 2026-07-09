#!/usr/bin/env bash
# Streaming fake SDK bridge: turn 1 succeeds (emits a success result), turn 2 CRASHES mid-turn (emits
# a partial text then exits non-zero WITHOUT a result) — e.g. an OOM/signal on a later turn. Proves
# the engine doesn't latch turn-1's success and finalize the whole session "done" when turn 2 crashed.
# Control lines ({"__bridge":"end"|"interrupt"}) are handled as in fake-sdk-bridge-stream.sh.
# Writes to $SDK_BRIDGE_LOG (SdkRunner sends the bridge's stdout to /dev/null).
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"fake-stream-crash-1","model":"claude-opus-4-8"}'
n=0
while IFS= read -r line; do
  case "$line" in
    *__bridge*end*) exit 0 ;;
    *__bridge*) continue ;;
  esac
  n=$((n + 1))
  if [ "$n" -ge 2 ]; then
    emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"partial turn 2"}}}'
    exit 1
  fi
  emit '{"type":"system","subtype":"init","session_id":"fake-stream-crash-1"}'
  emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"reply 1"}}}'
  emit '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
done
