# Commit-graph view Implementation Plan

> **For agentic workers:** Implement task-by-task, TDD. Steps use `- [ ]`. Spec:
> `docs/superpowers/specs/2026-06-20-commit-graph-view-design.md`.

**Goal:** Replace the per-file line diff viewer with a per-repo commit-history graph; tapping a commit shows
its changed-file list (no line diffs).

**Architecture:** Backend exposes `/commits` (recent history + session-commit flags + uncommitted node) and
`/commits/:sha/files` (name-status + numstat) per repo, reusing the existing `git()` helper. The app renders
the graph client-side (Canvas single-lane gutter) and opens a changed-file bottom sheet on tap.

**Tech Stack:** TypeScript/Fastify/vitest (backend); Kotlin/Compose/Ktor/JUnit (app).

## Global Constraints
- Backend: never call real `claude` in tests; `yarn test` runs `tsc --noEmit` then vitest — both must stay green.
- Backend: validate caller-supplied path segments — `repo` ∈ `s.repos`; `sha` matches `/^[0-9a-fA-F]{4,40}$/` or `=== "working"`.
- App: Ktor 2.3.12; Compose M3 pinned `1.4.0-alpha18`; verify with `gradle :app:testDebugUnitTest` (+ `compileDebugKotlin`). Build the APK only in the main checkout.
- Keep `engine.discard()` + `POST /api/sessions/:id/discard`.

---

## Part A — Backend (`agentic-dev`)

### Task A1: git helpers + `engine.commitGraph` / `engine.commitFiles`

**Files:**
- Modify: `server/engine/structuredDiff.ts` → trim to a git-info module: keep `git()` + name-status letter→`FileStatus` parsing; remove `parseHunks`, `fileDiff`, `structuredRepoDiff`, `structuredSessionDiff`, `DiffHunk`, `FileDiff`. Add `commitGraphForRepo(worktree, baseSha)` and `commitFilesForRepo(worktree, sha)`.
- Modify: `server/engine/engine.ts` — add `commitGraph(id)` and `commitFiles(id, repo, sha)` (off the event loop; remove `diff(id)`).
- Test: `server/engine/structuredDiff.test.ts` (rename concept to commit-graph) + `server/engine/engine.test.ts`.

**Interfaces (Produces):**
```ts
export interface CommitNode { sha: string; shortSha: string; parents: string[]; subject: string; author: string; at: number; isSession: boolean }
export interface RepoCommits { repo: string; commits: CommitNode[]; uncommitted: { added: number; modified: number; deleted: number } | null }
export interface CommitFile { path: string; status: FileStatus; additions: number; deletions: number }
// engine:
commitGraph(id: string): Promise<RepoCommits[]>
commitFiles(id: string, repo: string, sha: string): Promise<CommitFile[]>
```

- [ ] **Step 1 — failing test (commitGraphForRepo).** In a fixture git repo (use `makeTempGitRepo` + extra commits): assert `commitGraphForRepo(wt, baseSha)` returns commits newest-first, with `isSession=true` for commits after `baseSha`, `parents` populated, and `uncommitted` non-null after editing a tracked file.
- [ ] **Step 2 — run, see it fail.** `yarn test structuredDiff`.
- [ ] **Step 3 — implement.** Key commands (reuse `git()`):
  - commits: `git("-C",wt,"log","--no-color","-n","30","--pretty=%H%x1f%P%x1f%s%x1f%an%x1f%at","HEAD")`, split each line on `\x1f`; `shortSha=sha.slice(0,7)`; `parents=P.split(" ").filter(Boolean)`; `at=Number(at)*1000`.
  - session set: `git("-C",wt,"rev-list", `${baseSha}..HEAD`)` → `Set<sha>`; skip when `baseSha` falsy → empty set.
  - uncommitted: `git("-C",wt,"status","--porcelain")`; count lines by XY code → added (`A`/`??`), deleted (`D`), else modified; null when empty.
  - `commitFilesForRepo(wt, sha)`: if `sha==="working"` → `git diff --no-color --name-status HEAD` + `--numstat HEAD` (+ untracked from `status --porcelain` as added); else `git show --no-color --name-status --numstat <sha>`. Merge name-status + numstat by path (binary numstat `-`→0).
- [ ] **Step 4 — engine methods + test.** `commitGraph(id)`: `liveSession`-style guard is NOT needed (read-only); resolve each repo worktree (`repoWorktree(s,repo)`), `baseShas[repo]`, call `commitGraphForRepo`. `commitFiles(id,repo,sha)`: validate `repo ∈ s.repos` (throw `unknown repo`) and `sha` regex/`working` (throw `bad sha`); call `commitFilesForRepo`. Add an `engine.test.ts` case: a done session yields commits incl. the session commit. Remove `engine.diff` + its two tests (`engine.test.ts:562,638`).
- [ ] **Step 5 — green + commit.** `yarn test` green; `git commit -m "feat(engine): commit graph + commit files (replaces structured diff)"`.

### Task A2: routes `/commits` + `/commits/:sha/files`

**Files:** Modify `server/api/routes.ts` (replace the two diff routes); Test `server/api/server.test.ts`.

- [ ] **Step 1 — failing API test.** Login, create+finish a session (`doneId`), `GET /api/sessions/:id/commits` → 200 with `repos[0].commits` non-empty; `GET /api/sessions/:id/commits/:sha/files?repo=demo` for a real sha → 200 with `files`; `?repo=demo&sha=../etc` (via `/commits/..%2f/files`) or a non-hex sha → 400; unknown session → 404.
- [ ] **Step 2 — run, fail.** `yarn test server.test`.
- [ ] **Step 3 — implement routes.**
```ts
app.get<{Params:{id:string}}>("/api/sessions/:id/commits", async (req, reply) => {
  if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
  try { return { repos: await engine.commitGraph(req.params.id) }; }
  catch (err:any) { return reply.code(400).send({ error: String(err?.message ?? err) }); }
});
app.get<{Params:{id:string;sha:string};Querystring:{repo?:string}}>("/api/sessions/:id/commits/:sha/files", async (req, reply) => {
  if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
  const repo = req.query.repo ?? "";
  try { return { files: await engine.commitFiles(req.params.id, repo, req.params.sha) }; }
  catch (err:any) { return reply.code(400).send({ error: String(err?.message ?? err) }); }
});
```
- [ ] **Step 4 — green + commit.** Remove the old `/diff` + `/diff/files` routes. `yarn test` green; commit.

### Task A3: prune leftovers
- [ ] Remove any now-dead imports (`structuredSessionDiff`) from routes.ts; `yarn test` + `yarn typecheck` green; commit `chore: drop diff endpoints`.

---

## Part B — App (`agentic-dev-android`)

### Task B1: models + serialization test
**Files:** Modify `app/src/main/java/dev/agentic/data/net/Models.kt`; Test `app/src/test/java/dev/agentic/data/net/SessionSerializationTest.kt`.
- [ ] **Step 1 — failing test.** Decode representative JSON for `RepoCommits`/`CommitNode`/`CommitFile` (incl. `uncommitted:null`); assert fields.
- [ ] **Step 2 — add models.**
```kotlin
@Serializable data class CommitNode(val sha:String, val shortSha:String, val parents:List<String> = emptyList(), val subject:String = "", val author:String = "", val at:Long = 0, val isSession:Boolean = false)
@Serializable data class Uncommitted(val added:Int=0, val modified:Int=0, val deleted:Int=0)
@Serializable data class RepoCommits(val repo:String, val commits:List<CommitNode> = emptyList(), val uncommitted:Uncommitted? = null)
@Serializable data class CommitsResp(val repos:List<RepoCommits> = emptyList())
@Serializable data class CommitFile(val path:String, val status:String="modified", val additions:Int=0, val deletions:Int=0)
@Serializable data class CommitFilesResp(val files:List<CommitFile> = emptyList())
```
  Remove `DiffFile`, `DiffHunk`, `DiffFilesResp`.
- [ ] **Step 3 — green + commit.**

### Task B2: API + repo
**Files:** `data/net/AgenticApi.kt`, `KtorAgenticApi.kt`, `data/repo/FilesRepository.kt`.
- [ ] Replace `diffFiles(id)` with `commits(id): List<RepoCommits>` (`GET …/commits` → `CommitsResp.repos`) and `commitFiles(id, repo, sha): List<CommitFile>` (`GET …/commits/$sha/files?repo=$repo` → `CommitFilesResp.files`, repo URL-encoded). Repo: `commits(id)`/`commitFiles(...)` wrapped in `runCatchingOutcome`. Remove `diffFiles`. Compile.

### Task B3: CommitGraphViewModel + tests
**Files:** Create `app/src/main/java/dev/agentic/ui/tree/CommitGraphViewModel.kt`; Test `app/src/test/java/dev/agentic/ui/tree/CommitGraphViewModelTest.kt`. Remove `ui/diff/DiffViewModel.kt` + `DiffViewModelTest.kt`.
- [ ] **Step 1 — failing test (FakeAgenticApi):** load success populates `repos`; failure sets `error` (clean `userMessage()`); `loadFilesFor(repo,sha)` populates `detailFiles`; `discard()` calls repo + reloads.
- [ ] **Step 2 — implement** `MutableStateFlow<CommitGraphUiState(repos, loading, error, detail)>`; `init { load() }`; one-shot loads; cancellation rethrown by the repo's `runCatchingOutcome`.
- [ ] **Step 3 — green + commit.**

### Task B4: CommitGraphScreen + gutter + detail sheet
**Files:** Create `app/src/main/java/dev/agentic/ui/tree/CommitGraphScreen.kt`. (UI — verify via `compileDebugKotlin`.)
- [ ] Per-repo `Text(repo)` section header; a `LazyColumn` of rows: leading Canvas gutter (lane line + dot; `isSession` → accent; 2-parent → small merge curve), then `shortSha` (mono) + `subject` + `author` + `relativeTime(at)`. Top "Uncommitted changes (N)" row when `uncommitted != null` with a Discard `Button` (confirm dialog). Tap a row → `ModalBottomSheet` listing `CommitFile`s (status chip + `+a −d`), data from `loadFilesFor`. Error state → message + Retry.

### Task B5: nav + button swap + remove diff UI
**Files:** `ui/nav/AppNav.kt`, `ui/session/SessionScreen.kt`, `ui/home/AdaptiveHome.kt`; Remove `ui/diff/DiffPane.kt`.
- [ ] Rename route `@Serializable data class Diff` → `History`; `composable<History> { CommitGraphScreen(onBack=…) }`. Rename `onOpenDiff`→`onOpenHistory`; Session + AdaptiveHome icons → `Icons.Rounded.AccountTree` ("history"). Remove `DiffPane` import/usage and `Icons.Rounded.Difference`. `compileDebugKotlin` + `:app:testDebugUnitTest` green; commit.

---

## Self-review
- Spec coverage: /commits (A1,A2) ✓, /commits/:sha/files (A1,A2) ✓, uncommitted node (A1, B4) ✓, per-repo (A1,B4) ✓, validation (A1) ✓, removals (A3,B1,B5) ✓, discard kept (B3,B4) ✓, running+terminal (A1: no terminal guard) ✓, tests (every task) ✓.
- Types consistent across tasks (CommitNode/RepoCommits/CommitFile names match backend ↔ app).
