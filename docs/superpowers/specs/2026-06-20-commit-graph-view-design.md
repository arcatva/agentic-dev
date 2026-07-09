# Commit-graph view (replaces the diff viewer)

**Date:** 2026-06-20 · **Repos:** `agentic-dev` (backend) + `agentic-dev-android` (app)

## Goal

Remove the per-file line-by-line **diff viewer** and replace it with a **commit-history graph** of the
session's branch. Tapping a commit shows that commit's changed-file list (names + status + ± counts), but
**no line-level diff**. Multi-repo sessions show one graph **per repo**.

## Decisions (approved)

1. View = a `git log --graph`-style commit graph of the session branch.
2. Range = recent base history with session commits marked — `git log -n 30 HEAD`; commits in `baseSha..HEAD`
   are flagged `isSession`.
3. Tapping a commit → its changed-file list (path, A/M/D status, additions, deletions). No hunks.
4. Multi-repo → per-repo sections.
5. An **uncommitted-changes** node sits at the top of each repo (agent work is often uncommitted); it hosts
   the **Discard** action.
6. Available for **running and terminal** sessions (`git log` is read-only).
7. Graph gutter is a **single-lane** renderer (dot per commit + connecting line; 2-parent merges draw a
   simple branch/merge curve; rare complex merges degrade to a labeled dot). No full octopus-DAG engine.

## Backend (`agentic-dev`)

### New endpoints
- `GET /api/sessions/:id/commits` →
  ```
  { repos: [ { repo: string,
               commits: [ { sha, shortSha, parents: string[], subject, author, at:number, isSession:boolean } ],
               uncommitted: { added:number, modified:number, deleted:number } | null } ] }
  ```
  - Per repo: `git -C <worktree> log --no-color -n 30 --pretty=<fmt> HEAD` where the format yields full SHA,
    parents (`%P`), subject (`%s`), author name (`%an`), author epoch (`%at`).
  - `isSession` = sha ∈ `baseSha..HEAD` (compute the set via `git rev-list <baseSha>..HEAD`); a repo with a
    null/absent `baseSha` marks nothing.
  - `uncommitted` = counts from `git status --porcelain` (or `git diff --name-status HEAD` + untracked),
    null when the worktree is clean. (Staging-agnostic: count working-tree + untracked.)
- `GET /api/sessions/:id/commits/:sha/files?repo=<repo>` →
  `{ files: [ { path, status: "added"|"modified"|"deleted"|"renamed"|"unknown", additions:number, deletions:number } ] }`
  - For a real sha: `git show --no-color --name-status --numstat <sha>`.
  - For the uncommitted node: `sha = "working"` → worktree vs HEAD (`git diff --name-status --numstat HEAD`
    plus untracked as "added").

### Engine
- `commitGraph(id): Promise<RepoCommits[]>` and `commitFiles(id, repo, sha): Promise<CommitFile[]>`, run off
  the event loop (same pattern as the old `engine.diff`). Reuse the `git()` helper + name-status letter→status
  parsing already in `structuredDiff.ts`.
- **Validation:** `repo` must be one of `s.repos`; `sha` must match `/^[0-9a-fA-F]{4,40}$/` or be the literal
  `working` (mirrors the path-traversal hardening). Reject otherwise.

### Removals
- `GET /api/sessions/:id/diff` and `GET /api/sessions/:id/diff/files`.
- `engine.diff()`, `structuredSessionDiff` / `structuredRepoDiff` and the hunk parser in `structuredDiff.ts`
  (keep the small `git()` helper + name-status parser, relocated/kept as needed).
- Their tests. Keep `engine.discard()` and `POST /api/sessions/:id/discard`.

## App (`agentic-dev-android`)

### Removals
- `ui/diff/DiffPane.kt`, `ui/diff/DiffViewModel.kt`, `DiffViewModelTest`.
- `DiffFile` / `DiffHunk` / `DiffFilesResp` models, `diffFiles()` on the API + repo.

### Additions
- Models: `Commit(sha, shortSha, parents, subject, author, at, isSession)`, `RepoCommits(repo, commits,
  uncommitted)`, `Uncommitted(added, modified, deleted)`, `CommitFile(path, status, additions, deletions)`.
- API: `commits(id): List<RepoCommits>` and `commitFiles(id, repo, sha): List<CommitFile>` on `AgenticApi` /
  `KtorAgenticApi`; repo passthroughs on `FilesRepository` returning `Outcome`.
- `ui/tree/CommitGraphViewModel.kt` — one-shot load of `commits(id)` into `CommitGraphUiState(repos, loading,
  error)`; `loadFilesFor(repo, sha)` populates a detail sheet; `discard()` → `filesRepo.discard` then reload.
- `ui/tree/CommitGraphScreen.kt` — per-repo sections; each row = a Canvas graph gutter + short SHA + subject
  + author + relative time (reuse `domain/RelativeTime`); session commits accent-colored + a "session" chip;
  top "Uncommitted changes (N)" node when present. Tap a row → bottom sheet listing `CommitFile`s. Discard
  button on the uncommitted node (confirm dialog).
- Graph gutter: a `Modifier.drawBehind`/Canvas single-lane renderer keyed off `parents` for connecting lines
  and the simple 2-parent merge curve.

### Wiring
- Nav: rename `Diff(id)` route → `History(id)`; `composable<History> { CommitGraphScreen(...) }`.
- The Session screen's diff icon and `AdaptiveHome`'s right-pane diff icon become the history button using the
  already-imported `Icons.Rounded.AccountTree` (drop `Icons.Rounded.Difference`). `onOpenDiff` → `onOpenHistory`.

## Error handling
- Backend: a git failure / unknown repo / bad sha → 400 with a message (same shape as the old diff route);
  unknown session → 404. A repo whose worktree was discarded still lists commits from the source repo if
  resolvable, else returns an empty `commits` for that repo (no 500).
- App: load failure → `error` in the UI state with a clean message (reuse `AppError.userMessage()`); a Retry
  action. Cancellation rethrown (the structured-concurrency rule already in place).

## Testing
- Backend (`engine` + a fixture git repo with ≥2 commits and an uncommitted change): `commitGraph` returns
  per-repo commits with correct `isSession` flags and the uncommitted node; `commitFiles` returns name-status
  + numstat for a sha and for `working`; `sha`/`repo` validation rejects traversal/bad input; multi-repo
  yields multiple sections. API tests for the two new routes (200 shape, 400 on bad sha, 404 unknown session).
- App (JVM unit tests with `FakeAgenticApi`): `CommitGraphViewModel` load success/empty/error; `loadFilesFor`
  populates the sheet; `discard` reloads; serialization of the new models against representative JSON.
- Both suites green; backend `tsc --noEmit` gate stays green.

## Out of scope (YAGNI)
- Full multi-branch lane/octopus rendering; line-level diffs; commit search/filter; committing/checkout from
  the app; blame.
