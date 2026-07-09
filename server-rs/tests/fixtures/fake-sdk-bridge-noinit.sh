#!/usr/bin/env bash
# Fake SDK bridge that emits NO init event → the session never captures a claudeSessionId.
# Writes to $SDK_BRIDGE_LOG.
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"noinit"}}}'
emit '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
