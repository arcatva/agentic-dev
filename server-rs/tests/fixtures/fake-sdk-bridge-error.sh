#!/usr/bin/env bash
# Fake SDK bridge: emits a turn that fails with an error result (e.g. usage limit reached).
# Writes to $SDK_BRIDGE_LOG (SdkRunner sends the bridge's stdout to /dev/null).
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"err-sess-1"}'
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"partial work"}}}'
emit '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You'"'"'ve hit your session limit · resets 3:30pm (Pacific)"}'
