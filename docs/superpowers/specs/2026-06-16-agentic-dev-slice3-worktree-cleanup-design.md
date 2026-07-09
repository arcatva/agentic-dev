# agentic-dev — Slice 3 (Session worktree: review + cleanup) Design

**Date:** 2026-06-16
**Status:** Approved-to-proceed under the user's standing "keep going" (v1 deliberately excludes
the risky auto-merge; the spec is the review point).
**Builds on:** Slices 1–2. Every session creates a git worktree at
`~/src/agentic-worktrees/<repo>/<id>` on branch `agentic/<id>` that is **never removed** — with
real use these accumulate (disk + clutter). This slice adds review + cleanup.

## Goal

From the session view: **review** what a finished session changed (a diff), and **clean up** its
worktree — **Archive** (preserve the work on its branch, free the checkout) or **Discard**
(delete worktree + branch) — so worktrees stop accumulating.

## Scope

**In:** `GET /api/sessions/:id/diff`; engine `diff()` / `archiveWorktree()` / `discardWorktree()`;
a `baseSha` recorded per session (for an exact diff) + a `worktreeState` field; routes; UI (a
"View diff" panel + Archive/Discard actions on a finished session); dashboard worktree indicator.

**Out (deferred, deliberately):**
- **Auto-merge into the repo's default branch.** Merging `--dangerously-skip-permissions` agent
  output into real repos (incl. the cluster repos) is high-risk (conflicts, irreversible). v1
  does NOT do it. To land a session's work, the user reviews the diff and merges manually:
  `git -C ~/src/<repo> merge agentic/<id>` (Archive keeps the branch for exactly this).
- Conflict-resolution UI; multi-session batch cleanup; diffs for already-removed worktrees.

## Decisions locked

- **No auto-merge to main in v1** (see above).
- **Archive** = if the worktree has uncommitted changes, commit them to `agentic/<id>` first
  (nothing is lost), then `git worktree remove` (keep the branch). The branch + its commits
  persist and remain manually mergeable.
- **Discard** = `git worktree remove --force` + `git branch -D agentic/<id>` (throw it away).
- **Exact diff** needs the fork point: record `baseSha` (repo HEAD at `submit` time) on the
  session. Diff = the worktree's changes vs `baseSha`, **including uncommitted + untracked**.
- **`worktreeState`**: `live` (default) → `archived` | `discarded`. Once not `live`, the diff is
  unavailable and the actions are hidden.
- Cleanup is only allowed when the session is terminal (`done`/`failed`/`killed`) — never on a
  running session.

## Components / changes

| File | Change |
|---|---|
| `server/engine/types.ts` | `Session` gains `baseSha: string \| null` and `worktreeState: "live" \| "archived" \| "discarded"`. |
| `server/engine/store.ts` | Add `baseSha`, `worktreeState` columns (default `worktreeState='live'`); include in create/rowToSession; `reconcileOrphans` unchanged. |
| `server/engine/worktree.ts` | Add `archiveWorktree(repoPath, worktreePath, branch, message)` (commit-if-dirty then `worktree remove`) and `discardWorktree(repoPath, worktreePath, branch)` (`worktree remove --force` + `branch -D`); add `diffWorktree(worktreePath, baseSha): string` (intent-to-add untracked, `git diff baseSha`). |
| `server/engine/engine.ts` | `submit` records `baseSha = git -C <repo> rev-parse HEAD` before `createWorktree`. New `diff(id)`, `archive(id)`, `discard(id)` (validate terminal + `worktreeState==="live"`; update `worktreeState`). |
| `server/api/routes.ts` | `GET /api/sessions/:id/diff` → `{ diff }`; `POST /api/sessions/:id/archive`; `POST /api/sessions/:id/discard`. 404 unknown, 400 if running / already cleaned. |
| `web/src/api.ts` | `diff(id)`, `archive(id)`, `discard(id)`. `Session` type gains `worktreeState`. |
| `web/src/pages/SessionView.tsx` | When terminal + `worktreeState==="live"`: a "View diff" toggle (fetches + shows the patch, monospace) + **Archive** / **Discard** buttons. After cleanup, show the `worktreeState` and hide the actions. |
| `web/src/pages/Dashboard.tsx` | Show each session's `worktreeState` (e.g. a small chip), so live worktrees are visible. |

## Data flow

```
finished session (worktreeState=live)
  → "View diff" → GET /api/sessions/:id/diff → git diff <baseSha> (incl. uncommitted/untracked) → patch shown
  → "Archive" → POST /archive → commit-if-dirty on agentic/<id>, git worktree remove → worktreeState=archived
       (branch kept; user may later: git -C ~/src/<repo> merge agentic/<id>)
  → "Discard" → POST /discard → git worktree remove --force + git branch -D → worktreeState=discarded
```

## Error handling

- diff/archive/discard on unknown id → `404`.
- on a running/pending session → `400 busy`.
- on an already-archived/discarded session → `400` (no live worktree).
- `git worktree remove` failing (e.g. path already gone) → treat as already-removed: set the
  state and return success (idempotent), don't 500.
- diff when `baseSha` is null (older sessions created before this slice) → fall back to
  `git diff HEAD` in the worktree (uncommitted only) and note the limitation in the response.

## Testing (fake binary + temp git repos, zero credit)

- **worktree.ts:** `diffWorktree` shows a created/edited file vs base (incl. untracked);
  `archiveWorktree` commits a dirty worktree then removes it, branch still exists with the commit;
  `discardWorktree` removes worktree + deletes branch; both idempotent if the path is already gone.
- **engine:** `submit` records a non-null `baseSha`; `diff(id)` returns a patch containing the
  agent's change; `archive(id)` sets `worktreeState=archived` and the worktree dir is gone but the
  branch remains; `discard(id)` sets `discarded` and the branch is gone; both reject a running
  session and an already-cleaned session.
- **routes:** the three endpoints' 200/400/404 paths.

## Success criteria

1. A finished session shows its diff in the UI (the file(s) the agent changed).
2. Archive removes the worktree directory but keeps `agentic/<id>` (its commit is intact and
   manually mergeable); `worktreeState` shows `archived`.
3. Discard removes both the worktree and the branch; `worktreeState` shows `discarded`.
4. Cleanup is rejected on a running session.
5. Backend tests green on the fake binary; web builds.

## Verification (non-disruptive)

Built + tested + a Playwright check on a **separate port** (not `:7420`), in an isolated git
worktree of agentic-dev. The live `:7420` systemd service is restarted only on the user's
redeploy go-ahead.
