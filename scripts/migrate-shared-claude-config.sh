#!/usr/bin/env bash
#
# One-time migration for the "shared ~/.claude config dir" cutover.
#
# Before: each agentic session ran with CLAUDE_CONFIG_DIR=<worktree>/.claude-config, so claude kept
# its transcripts (--resume context) and workflow data under <worktree>/.claude-config/projects/.
# After:  all sessions run with CLAUDE_CONFIG_DIR=~/.claude, so claude reads/writes ~/.claude/projects/.
#
# The project "slug" under projects/ is derived from the session cwd, which is unchanged by the
# cutover — so a session's data simply needs to move from
#     <worktree>/.claude-config/projects/<slug>/...
# to
#     ~/.claude/projects/<slug>/...
# This script merge-copies it WITHOUT overwriting anything already present (so any data the user
# created by running `claude` manually in the same worktree wins). Idempotent; safe to re-run.
#
# It does NOT touch credentials: ~/.claude/.credentials.json is the single shared, canonical token.
# The old per-session .claude-config dirs are left in place as a backup (the engine no longer reads
# them; they vanish when the session/worktree is deleted).
#
# Usage:
#   scripts/migrate-shared-claude-config.sh            # do the migration
#   scripts/migrate-shared-claude-config.sh --dry-run  # show what would be copied, change nothing
#
# Env overrides (match the server's AGENTIC_* config):
#   AGENTIC_CLAUDE_CONFIG_BASE (default: ~/.claude)  — the SHARED dest base
#   AGENTIC_WORKTREES_ROOT     (default: ~/src/agentic-worktrees)
#
# NOTE: do NOT key the destination off $CLAUDE_CONFIG_DIR. Inside an agentic session that variable
# points at the session's OWN per-session .claude-config (a migration SOURCE), not the shared base.
# We resolve the dest exactly like the server's claude_config_base: AGENTIC_CLAUDE_CONFIG_BASE | ~/.claude.

set -euo pipefail

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

CLAUDE_HOME="${AGENTIC_CLAUDE_CONFIG_BASE:-$HOME/.claude}"
WORKTREES="${AGENTIC_WORKTREES_ROOT:-$HOME/src/agentic-worktrees}"
DEST="$CLAUDE_HOME/projects"

echo "shared-config migration"
echo "  worktrees : $WORKTREES"
echo "  dest      : $DEST"
echo "  mode      : $([ "$DRY_RUN" = 1 ] && echo DRY-RUN || echo APPLY)"
echo

[ "$DRY_RUN" = 1 ] || mkdir -p "$DEST"

shopt -s nullglob
slugs_done=0
files_copied=0

for cfg in "$WORKTREES"/*/.claude-config/projects; do
  [ -d "$cfg" ] || continue
  sess=$(printf '%s\n' "$cfg" | sed -E 's#.*/agentic-worktrees/([^/]+)/.*#\1#')
  for slug in "$cfg"/*; do
    [ -d "$slug" ] || continue
    name=$(basename "$slug")
    # count files that don't already exist at the destination (what we'd actually copy)
    new=$(cd "$slug" && find . -type f 2>/dev/null | while read -r rel; do
            [ -e "$DEST/$name/$rel" ] || echo "$rel"
          done | wc -l)
    echo "  [$sess] $name  (+$new new files)"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p "$DEST/$name"
      cp -rn "$slug"/. "$DEST/$name"/ 2>/dev/null || true
    fi
    slugs_done=$((slugs_done + 1))
    files_copied=$((files_copied + new))
  done
done

echo
if [ "$DRY_RUN" = 1 ]; then
  echo "DRY-RUN: would merge $slugs_done slug dir(s), ~$files_copied new file(s) into $DEST"
else
  echo "done: merged $slugs_done slug dir(s), ~$files_copied new file(s) into $DEST (no-clobber)"
fi
