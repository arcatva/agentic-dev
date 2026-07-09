#!/usr/bin/env bash
# Fake SDK bridge (test double for sdk-bridge.mjs), driven by SdkRunner exactly like production.
# The bridge appends canned stream-json to $SDK_BRIDGE_LOG itself — SdkRunner routes the bridge's
# stdout to /dev/null, so output MUST go to the log file (this is the key difference from the old
# fake-claude.sh, which wrote to stdout). One-shot: emits one happy-path turn and exits.
# Honors FAKE_CLAUDE_SLEEP to delay before the result (to test concurrency/kill).
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"fake-sess-123","model":"claude-opus-4-8"}'
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Hello "}}}'
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"world"}}}'
sleep "${FAKE_CLAUDE_SLEEP:-0}"
emit '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.0042}'
