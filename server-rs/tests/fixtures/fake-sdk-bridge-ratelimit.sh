#!/usr/bin/env bash
# Fake SDK bridge: a turn that fails with a TRANSIENT server rate-limit (not the user's quota). The
# text self-identifies as "not your usage limit" — the engine must tag errorKind=rate_limited, not
# usage_limit. Writes to $SDK_BRIDGE_LOG.
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"rl-sess-1"}'
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"partial work"}}}'
emit '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"API Error: Server is temporarily limiting requests (not your usage limit) Rate limited"}'
