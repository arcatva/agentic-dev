# Periodic Session Title Refresh — design

Date: 2026-06-23
Status: draft (awaiting user review)
Repo: `agentic-dev` (server-rs)

## Problem

The auto-title feature (`specs/2026-06-23-auto-session-title-design.md`) generates a stable session title at `submit_session` time. That title is then frozen — the session list always shows what the session started on. But sessions that drift in direction (started as "fix bug X", now optimizing performance) keep a stale title forever.

## Goal

Every 5 user messages, give the session one chance to update its title based on the recent conversation. If the new haiku call decides the topic has shifted, write the new title; otherwise leave the existing one.

## Non-goals

- Real-time title updates on every message.
- A separate `summary` field for list previews.
- Client-driven retitle UI (the existing `setTitle=true` escape hatch is unchanged).
- Cross-session title similarity or de-duplication.

## Design

### Trigger

`Engine::follow_up` already increments `Activity.turns` (per-session user-message counter). When `turns % 5 == 0` **after** incrementing, the engine kicks off a fire-and-forget retitle attempt. The increment happens before the trigger check, so:

- After the 5th, 10th, 15th, … user message of a session, a retitle attempt is enqueued.
- The `follow_up` API call returns immediately; the retitle runs in a `tokio::spawn` background task.

### Generation

A new function in `server-rs/src/engine/title.rs`:

```rust
pub async fn maybe_retitle(
    bin: &str,
    current_title: &str,
    recent_messages: &[(String, String)], // (role, text); role in {"user","assistant"}
    env: &HashMap<String, String>,
    cwd: &Path,
) -> Option<String>
```

Inputs:
- `bin`, `env`, `cwd`: same plumbing as `generate_title`.
- `current_title`: the existing `sessions.prompt`.
- `recent_messages`: the last **10** entries parsed from the session log (5 user + 5 assistant typical), capped at 4 KiB total of body text to bound the prompt size.

Returns:
- `Some(new_title)` if haiku says `{"change": true, "title": "..."}` AND the new title passes `is_valid_title` AND differs from `current_title`.
- `None` otherwise (no change, parse failure, validation failure, timeout, non-zero exit, subprocess error).

System prompt for haiku (new constant, separate from `TITLE_SYSTEM_PROMPT`):

```
你的任务是判断这个 session 的标题是否需要更新。

输入包含:
- 当前标题: ...
- 最近 10 条消息 (按时间顺序)

输出必须是以下 JSON 之一,不要其他内容,不要 markdown:
- 不需要改: {"change": false}
- 需要改:   {"change": true, "title": "<5-12 个汉字的新标题>"}

判断标准:
- 标题要反映"session 当前在做什么",而不是第一句话或最后一句话
- 如果当前标题仍然准确,输出 {"change": false}
- 如果话题已经明显改变(比如从修 bug 变成做性能优化),输出新标题
- 新标题跟旧标题不能完全相同
- 不要给"ok"、"继续"这种短回复单独起标题
```

### Where messages come from

The retitle task reads `Store::read_log(id)` (already public) and parses the JSONL to extract:

- User messages: lines with `{"type":"agentic_prompt","text":...}`
- Assistant text: lines containing `text_delta` deltas or `Text` events (we extract the joined text per event; for the log we use the raw line if it contains the text directly)

The parser lives next to `maybe_retitle` in `title.rs` (~30 lines). It caps at 10 entries and at 4 KiB of joined body text, then builds the `(role, text)` list.

### Fire-and-forget

In `follow_up`, after the existing turn-counter increment and before returning:

```rust
if self.0.cfg.retitle_enabled && turns_after_increment % 5 == 0 {
    let engine = self.clone(); // Engine is Arc-cloneable
    let id = id.to_string();
    tokio::spawn(async move {
        if let Some(new) = engine.maybe_retitle_session(&id).await {
            // Silent write; no propagation to subscribers (title is metadata).
            if let Err(e) = engine.0.store.update(&id, SessionPatch {
                prompt: Some(new),
                ..Default::default()
            }).await {
                tracing::warn!("[engine] retitle store.update failed: {e}");
            }
        }
    });
}
```

`maybe_retitle_session(id)` is a small helper on `Engine` that does the read-log → parse → call `title::maybe_retitle` chain. The cloned `Engine` (`Arc<EngineInner>`) shares the store and config cheaply.

Subscribers do not receive a retitle event. Title changes are metadata, not part of the conversation stream — clients can refresh the session list to see the new title.

### Failure modes

| Condition                                              | Title used            |
| ------------------------------------------------------ | --------------------- |
| haiku says `{"change": false}`                        | unchanged             |
| haiku says `{"change": true, "title": "..."}` valid   | new title             |
| haiku says new title == current title                 | unchanged (de-dup)    |
| JSON parse failure                                     | unchanged             |
| `is_valid_title` rejects new title                     | unchanged             |
| Timeout (>5s)                                          | unchanged             |
| Non-zero exit                                          | unchanged             |
| Empty stdout                                           | unchanged             |
| Log file missing / unreadable / no recent messages     | unchanged (no retitle attempt) |

In every failure path the title is exactly what it was before. No data loss; no spurious rewrite.

### Configuration

Add one bool field to `EngineConfig` and `Config`:

```rust
pub retitle_enabled: bool, // default true; flip via AGENTIC_RETITLE=off
```

`Engine::new` reads it from `Config::load()` exactly like other fields. Tests default it to `true` (most behavior unchanged) and a single test flips it to `false` to verify the gate.

### Validation

New title still goes through `is_valid_title` (existing helper): ≤24 chars, no leading `#`/`>`/`` ` ``, no newlines, non-empty after trim. The system prompt asks for 5–12 characters but the validator enforces the 24-char hard limit so a creative haiku that returns 20 chars still passes.

## Testing

Unit tests in `engine/title.rs`:

1. `maybe_retitle_returns_none_on_change_false`
2. `maybe_retitle_returns_some_title_on_valid_change`
3. `maybe_retitle_returns_none_on_invalid_json`
4. `maybe_retitle_returns_none_on_validation_failure`
5. `maybe_retitle_returns_none_when_new_equals_current`
6. `maybe_retitle_times_out_after_5s_on_slow_binary`
7. `parse_recent_messages_returns_last_10_user_and_assistant`

Integration tests in `engine/tests.rs`:

8. `follow_up_retitles_after_5th_message_on_change`
9. `follow_up_does_not_retitle_when_retitle_disabled`
10. `follow_up_does_not_retitle_when_change_false`
11. `followup_does_not_block_on_slow_retitle_subprocess` (the live-inject branch should return immediately even if the retitle subprocess is still running)

New fixtures in `server-rs/tests/fixtures/`:

- `fake-claude-title-retitle-keep.sh` — outputs `{"change": false}\n`, exits 0
- `fake-claude-title-retitle-change.sh` — outputs `{"change": true, "title": "性能优化阶段"}\n`, exits 0
- `fake-claude-title-retitle-badjson.sh` — outputs `not json`, exits 0
- `fake-claude-title-retitle-slow.sh` — `sleep 10` then outputs `{"change": true, "title": "too late"}`

## Risks

- **Cost**: ~$0.0005 per attempt, ~10 attempts/day for active users, ~$0.15–0.30/month — negligible vs main run cost. If usage patterns shift (lots of long sessions), revisit.
- **Latency**: retitle is `tokio::spawn`-ed, so `follow_up` API latency is unchanged. Worst case the title is "stale" by up to 5 turns plus 5 seconds.
- **Log parsing**: depends on the JSONL format `Store::append_log` already produces. If the format changes, the parser needs updating. Mitigation: keep the parser local to `title.rs` and test it with hand-written log lines.
- **Race with `setTitle=true` follow-up**: if a user explicitly renames between a retitle's `read_log` and `store.update`, the rename wins (it lands later). Acceptable.

## Out of scope (future ideas)

- Event published to subscribers on title change (for live UI updates).
- Per-message retry if `change=true` but title is invalid.
- Adaptive cadence (skip retitle for short sessions).
