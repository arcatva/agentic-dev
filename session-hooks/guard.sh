#!/usr/bin/env bash
# PreToolUse(Bash) guardrail.
#
# Hard-blocks a few irreversible / forbidden operations. These were previously
# only PROSE house-rules in CLAUDE.md; this makes them deterministic. Crucially,
# PreToolUse hooks still fire under --dangerously-skip-permissions, so this works
# without changing the permission mode.
#
# Protocol: exit 2 = block the tool call; stderr is shown back to the agent.
# Exit 0 = allow. Keep the rule set SMALL and high-confidence to avoid blocking
# legitimate work.

set -u

payload=$(cat)
tool=$(echo "$payload" | jq -r '.tool_name // empty' 2>/dev/null)
[ "$tool" = "Bash" ] || exit 0
cmd=$(echo "$payload" | jq -r '.tool_input.command // empty' 2>/dev/null)
[ -n "$cmd" ] || exit 0

# Scan with quoted strings stripped, so prose inside a commit message / -m / echo
# (e.g. the word ".tfvars" or "--force" in a message) never trips a rule. Rules
# below match command *syntax*, which lives outside quotes.
scan=$(printf '%s' "$cmd" | tr '\n' ' ' | sed -E 's/"[^"]*"//g' | sed -E "s/'[^']*'//g")

block() { echo "⛔ BLOCKED by guard.sh: $1" >&2; exit 2; }

# 1. Force-push to master/main (never force-push protected branches).
if echo "$scan" | grep -qE 'git[[:space:]]+push' \
   && echo "$scan" | grep -qE '(--force-with-lease|--force|[[:space:]]-f([[:space:]]|$))' \
   && echo "$scan" | grep -qE '(master|main)'; then
  block "force-push touching master/main is forbidden (house rule). Push without --force, or use a branch."
fi

# 2. Staging/committing local-only secret files. Plaintext *.tfvars is forbidden;
#    the GPG-ENCRYPTED *.tfvars.gpg backup IS allowed (mirrors terraform.tfstate.gpg).
tfvars_scan=$(printf '%s' "$scan" | sed -E 's/\.tfvars\.gpg//g')
if echo "$scan" | grep -qE 'git[[:space:]]+(add|commit)' \
   && echo "$tfvars_scan" | grep -qE '\.tfvars'; then
  block "staging/committing plaintext *.tfvars is forbidden — keep it local; commit only the encrypted *.tfvars.gpg (house rule)."
fi

exit 0
