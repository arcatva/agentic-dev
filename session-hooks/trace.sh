#!/usr/bin/env bash
# PostToolUse trace hook (observability).
#
# Appends one compact JSONL line per tool call to
# .claude/logs/trace-YYYY-MM-DD.jsonl, so harness metrics (tool mix, volume,
# edit-error rate, deploys/session) are computable after the fact — the skillset
# was previously flying blind on what the agent actually did.
#
# Privacy: for Bash it logs tool_input.description (a clean human summary), NOT
# the command, so tokens/secrets in commands never hit the log. For Edit/Write/
# Read it logs the file path. Never blocks (always exit 0).
#
# Analyse later, e.g.:
#   jq -r .tool .claude/logs/trace-*.jsonl | sort | uniq -c        # tool mix
#   jq 'select(.err)' .claude/logs/trace-*.jsonl                   # edit failures

set -u

payload=$(cat)
logdir="$CLAUDE_PROJECT_DIR/.claude/logs"
mkdir -p "$logdir" 2>/dev/null || exit 0
ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
day=$(date -u +%Y-%m-%d)

echo "$payload" | jq -c --arg ts "$ts" '
  {
    ts: $ts,
    tool: (.tool_name // "?"),
    detail: (
      if (.tool_name == "Bash")
      then ((.tool_input.description // "") | .[0:120])
      else (.tool_input.file_path // .tool_input.filePath // "")
      end
    ),
    # err is best-effort: catches the known silent-failure signatures (same set
    # verify-edit.sh uses). Not a reliable Bash exit-code signal.
    err: ((.tool_response | tostring)
          | test("Internal error|tool_use_error|has not been read yet|String to replace not found|Tool result missing"))
  }' >> "$logdir/trace-$day.jsonl" 2>/dev/null

exit 0
