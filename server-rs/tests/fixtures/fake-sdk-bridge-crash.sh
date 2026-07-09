#!/usr/bin/env bash
# Fake SDK bridge: a turn that dies abruptly (non-zero exit) WITHOUT emitting an error result —
# e.g. a crash, OOM-kill, or signal. The engine must fall back to a neutral "interrupted"/crashed
# message, NOT assert "usage limit reached". Writes to $SDK_BRIDGE_LOG.
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"crash-sess-1"}'
emit '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"partial work"}}}'
exit 1
