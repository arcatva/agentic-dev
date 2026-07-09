#!/usr/bin/env bash
# Fake SDK bridge: a turn that spawns a subagent (Agent tool) AND runs a regular tool (Bash), then
# delivers BOTH of their tool_results. Used to verify the engine persists an `agent_result` marker
# ONLY for the spawned-agent result (tu_agent), not for the plain tool (tu_bash).
# Writes to $SDK_BRIDGE_LOG.
set -euo pipefail
LOG="${SDK_BRIDGE_LOG:?SDK_BRIDGE_LOG required}"
emit() { printf '%s\n' "$1" >>"$LOG"; }
emit '{"type":"system","subtype":"init","session_id":"fake-sess-ar","model":"claude-opus-4-8"}'
emit '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_agent","name":"Agent","input":{"subagent_type":"Explore","description":"search the code"}}]}}'
emit '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_bash","name":"Bash","input":{"command":"ls"}}]}}'
emit '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_agent","content":"AGENT OUTPUT"}]}}'
emit '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_bash","content":"bash stdout"}]}}'
emit '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
