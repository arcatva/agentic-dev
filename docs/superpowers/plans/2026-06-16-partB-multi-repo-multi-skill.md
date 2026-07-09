# Part B — agentic-dev multi-repo + multi-skill + GitHub clone — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or
> executing-plans. TDD throughout; tests use the fake `claude` (`server/test/fixtures/fake-claude.sh`),
> never the real binary. Steps use checkbox (`- [ ]`).

**Goal:** A session selects **multiple repos** (worktree each) + **multiple skills** (per-session
config loads only those) + clones a missing repo from GitHub on demand.

**Architecture:** `Session` carries `repos: string[]` + `skills: string[]`; one session dir holds a
worktree per repo. The engine clones absent repos, then builds a per-session `CLAUDE_CONFIG_DIR`
(validated Task-0 recipe) and spawns claude there. API + web gain multi-select.

**Spec:** `docs/superpowers/specs/2026-06-16-agentic-dev-entry-point-design.md` (rev 2; Task-0 spike PASSED).

**Invariants across tasks:** column names `repos`,`skills` (JSON text); branch `agentic/<id>`;
session dir `<worktreesRoot>/<id>/`; per-repo worktree `<id>/<repo>`; back-compat: old rows → `repos=[repo]`, `skills=[]`.

---

## Slice 1 — multi-repo worktrees + clone-on-demand

### Task 1.1: store — add `repos` + `skills` columns (migration)
**Files:** `server/engine/store.ts`, `server/engine/store.test.ts`
- [ ] **Step 1: failing test** — open a store on a table WITHOUT `repos`/`skills`, assert
  `create({...repos:["a","b"],skills:["x"]})` round-trips and an old single-`repo` row reads back
  `repos=["<repo>"]`, `skills=[]`.
- [ ] **Step 2:** run → fails (`no such column: repos`).
- [ ] **Step 3:** add `repos TEXT`, `skills TEXT` to `ADDED_COLUMNS`; store as `JSON.stringify`; in
  `rowToSession` parse `repos = r.repos ? JSON.parse(r.repos) : (r.repo ? [r.repo] : [])`, `skills =
  r.skills ? JSON.parse(r.skills) : []`. Keep the legacy `repo` column populated (`repos[0]`) for
  back-compat reads.
- [ ] **Step 4:** run → pass; **Step 5:** `yarn test`; **Step 6:** commit.

### Task 1.2: types — `Session.repos`/`skills`
**Files:** `server/engine/types.ts`
- [ ] Add `repos: string[]; skills: string[];` to `Session` (keep `repo: string` = `repos[0]` for
  compatibility). `CreateInput` gains `repos`, `skills`. Update `EngineConfig` with `gitOrg` (default
  `"arcatva"`). Commit (compiles; covered by 1.1 test).

### Task 1.3: worktree — session dir helpers
**Files:** `server/engine/worktree.ts`, `server/engine/worktree.test.ts`
- [ ] **Step 1: failing test** — `createSessionWorktrees(repoPaths, root, id)` creates `<root>/<id>/<repo>`
  for each repo on branch `agentic/<id>`, returns `[{repo, worktreePath, baseSha}]`.
- [ ] **Steps 2-4:** implement (loop `createWorktree` per repo into the shared `<id>/` dir; compute
  baseSha per repo); `removeSessionWorktrees(id)` removes all + prunes each origin. Test green.
- [ ] **Step 5:** `diffWorktree`/`archive`/`discard` already per-path — engine calls them per repo.
- [ ] **Step 6:** commit.

### Task 1.4: clone-on-demand
**Files:** `server/engine/repos.ts`, `server/engine/repos.test.ts`
- [ ] **Step 1: failing test** — `ensureLocal(repo, srcRoot, gitOrg, cloneFn)` returns the path if
  `<srcRoot>/<repo>/.git` exists; else calls `cloneFn(url, dest)` (injected, so tests don't hit
  network) and returns the dest. URL = `https://github.com/<gitOrg>/<repo>.git`.
- [ ] **Steps 2-4:** implement + test (existing repo → no clone; absent → cloneFn called once).
- [ ] **Step 5:** `listRepos` gains a `remote` source: `listRemoteRepos(gitOrg)` shells
  `gh repo list <org> --json name -L 200` (wrapped, injectable; empty array on failure). Test with a
  fake `gh`. **Step 6:** commit.

### Task 1.5: engine — `submit(repos[], skills[], prompt)`
**Files:** `server/engine/engine.ts`, `server/engine/engine.test.ts`
- [ ] **Step 1: failing test** (fake claude) — `submit(["agentic-dev"], [], "hi")` returns an id;
  session has `repos=["agentic-dev"]`, a worktree under `<id>/agentic-dev`, runs to `done`.
- [ ] **Step 2-4:** rewrite `submit`: `ensureLocal` each repo → `createSessionWorktrees` → store.create
  with `repos`,`skills`,`worktreePath=<sessionDir>` → enqueue → pump. `start()` spawns with
  `cwd=sessionDir`. Keep `followUp` working (resume in the same session dir). `diff(id)` concatenates
  per-repo diffs (labelled). `archive`/`discard`/`kill` iterate the session's repos. Tests green.
- [ ] **Step 5:** `yarn test`; **Step 6:** commit.

### Task 1.6: API — skills + remote repos + repos[] body
**Files:** `server/api/routes.ts`, `server/api/server.test.ts`
- [ ] **Step 1: failing tests** — `GET /api/skills` → `[{name,description}]` from `~/.claude/skills`;
  `GET /api/repos` → `{local:[...],remote:[...]}`; `POST /api/sessions {repos:["x"],skills:[],prompt}`
  → `{id}`; legacy `{repo,prompt}` still works.
- [ ] **Steps 2-4:** implement: a `listSkills(skillsDir)` helper (read each `*/SKILL.md` frontmatter
  name+description); `/api/repos` returns the `{local,remote}` shape; `/api/sessions` accepts `repos[]`
  OR legacy `repo`, `skills[]` optional. Validate ≥1 repo + prompt. Tests green.
- [ ] **Step 5:** `yarn test`; **Step 6:** commit.

### Task 1.7: web — repo multi-select
**Files:** `web/src/pages/NewRequest.tsx`, `web/src/api.ts`
- [ ] Replace the single-repo `TextField select` with a **multi-select** (MUI `Autocomplete
  multiple` or checkbox list) over `{local, remote}` repos — remote tagged `(clone)`. `api.repos()`
  returns the new shape; `api.submit(repos, skills, prompt)`. (skills picker is Slice 2; pass `[]`.)
  `SessionView` shows the session's `repos`. Manual browser check: create a 2-repo session.
- [ ] Commit.

### Slice 1 checkpoint
`yarn test` green; create a real 2-repo session via the UI (e.g. `agentic-dev` + a small repo); confirm
two worktrees under `~/src/agentic-worktrees/<id>/`. Restart the service to run the new code.

---

## Slice 2 — multi-skill (per-session config dir)

### Task 2.1: per-session config dir builder (Task-0 recipe)
**Files:** `server/engine/claudeConfig.ts`, `server/engine/claudeConfig.test.ts`
- [ ] **Step 1: failing test** — `buildSessionConfigDir(baseClaudeDir, chosenSkills, destDir)` creates
  `destDir` symlinking `.credentials.json, CLAUDE.md, settings.json, plugins, memory` from
  `baseClaudeDir`, and `destDir/skills/<name>` → `baseClaudeDir/skills/<name>` for each chosen skill
  ONLY. Assert the skills dir contains exactly the chosen set.
- [ ] **Steps 2-4:** implement (symlink the fixed set if present; mkdir `skills/`; symlink chosen).
  Test with a fake `~/.claude`. Green. **Step 6:** commit.

### Task 2.2: spawner — `CLAUDE_CONFIG_DIR`
**Files:** `server/engine/spawner.ts`, `server/engine/spawner.test.ts`
- [ ] **Step 1: failing test** — `spawnClaude({..., claudeConfigDir})` sets `env.CLAUDE_CONFIG_DIR`;
  assert via a fake claude that echoes its env.
- [ ] **Steps 2-4:** add `claudeConfigDir?` to `SpawnOptions`; set the env when provided. Green. Commit.

### Task 2.3: engine wires skills → config dir
**Files:** `server/engine/engine.ts`, `server/engine/engine.test.ts`
- [ ] **Step 1: failing test** — `submit([...], ["rke2-ops"], ...)` builds a session config dir (in the
  session dir, e.g. `<sessionDir>/.claude-config`) with only `rke2-ops` and passes its path to spawn.
- [ ] **Steps 2-4:** in `start()`, if `skills.length`, `buildSessionConfigDir(~/.claude, skills,
  <sessionDir>/.claude-config)` and pass `claudeConfigDir`. `skills=[]` → omit (all load = today).
  Green. Commit.

### Task 2.4: web — skill multi-select
**Files:** `web/src/pages/NewRequest.tsx`, `web/src/api.ts`
- [ ] Add a **skill checkbox list** from `api.skills()` (name + description); selected → `submit(repos,
  skills, prompt)`. `SessionView` shows the session's `skills`. Manual browser check: a session with
  only `tenants-dev` + `rke2-ops` chosen. Commit.

### Slice 2 checkpoint
`yarn test` green; real session with a curated skill set; restart service; verify in the running app
that only the chosen skills are active (ask the session to list its skills).

---

## Out of scope
Auto-merge from UI; per-repo branch choice; non-`arcatva` orgs; plugin (superpowers) toggling.

## Rollback
All work on a feature branch / worktree of agentic-dev; the live service runs master until merged +
service restart. Revert = don't merge (or `git revert`).
