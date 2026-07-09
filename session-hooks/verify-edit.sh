#!/usr/bin/env bash
# PostToolUse hook: catches silent Edit/Write failures.
#
# The Edit tool can return "Internal error", "<tool_use_error>", "String to replace
# not found", or "File has not been read yet" — while still looking superficially
# like progress. Without a hook, the agent sometimes moves on as if the file
# changed, leaving state inconsistent.
#
# This hook grep's the tool_response for known failure signatures and prints a
# loud warning on stdout. Claude Code appends hook stdout to the agent's next
# context as a system reminder, so the agent is forced to see it and verify.
#
# Exit always 0 — we don't want to block the tool pipeline, just alert.

set -u

payload=$(cat)

tool=$(echo "$payload" | jq -r '.tool_name // empty' 2>/dev/null || echo "")

case "$tool" in
    Edit|Write|NotebookEdit) ;;
    *) exit 0 ;;
esac

response=$(echo "$payload" | jq -r '.tool_response | tostring' 2>/dev/null || echo "")

if echo "$response" | grep -qE 'Internal error|tool_use_error|has not been read yet|String to replace not found|Tool result missing'; then
    file=$(echo "$payload" | jq -r '.tool_input.file_path // .tool_input.filePath // "<unknown>"' 2>/dev/null)
    cat <<EOF
⚠️  HOOK: $tool on $file returned an error signature. The file likely did NOT change.
   Verify the intended content is present (grep / Read the edited region) before
   treating this step as complete. If the change is missing, retry the $tool call
   or Read the file first if the error was "File has not been read yet".
EOF
fi

exit 0
