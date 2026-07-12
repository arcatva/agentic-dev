use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::title_client::{InMemoryTitleGenerator, TitleGenerator};

/// Integration tests for submit_session title generation and
/// follow_up periodic retitle. All paths go through the
/// `TitleGenerator` trait; tests inject an `InMemoryTitleGenerator`
/// via `EngineOverrides.title_generator`.

async fn submit_with_generator(generator: Arc<dyn TitleGenerator>) -> (Engine, String) {
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(generator),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    (e, id)
}

/// submit_session spawns a fire-and-forget title task that lands
/// "性能优化阶段" on success. Poll until it does (or timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_session_uses_generator_title() {
    let (e, id) = submit_with_generator(Arc::new(InMemoryTitleGenerator::returning_title(
        "性能优化阶段",
    )))
    .await;
    let mut landed = false;
    for _ in 0..40 {
        if e.get(&id).await.unwrap().prompt == "性能优化阶段" {
            landed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(landed, "submit-time title never landed");
    assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
}

/// If the generator returns `None` (or returns garbage that fails
/// validation), the original prompt is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_session_keeps_prompt_when_generator_returns_none() {
    let (e, id) = submit_with_generator(Arc::new(
        InMemoryTitleGenerator::returning_none_for_retitle(),
    ))
    .await;
    // Even after waiting for the title task to run, the prompt is
    // unchanged because the generator returned None.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(e.get(&id).await.unwrap().prompt, "first prompt");
}

/// submit_session returns synchronously — the title task is
/// fire-and-forget on tokio::spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_session_returns_immediately() {
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(Arc::new(InMemoryTitleGenerator::returning_title(
                "性能优化阶段",
            ))),
            ..Default::default()
        },
    )
    .await;
    let started = std::time::Instant::now();
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "submit_session must not block on title generation; took {elapsed:?}"
    );
    wait_status(&e, &id, "done").await;
}

/// follow_up on the 5th turn spawns a retitle task. The in-memory
/// generator is configured to return a NEW title (different from the
/// submit-time title so dedup doesn't kick in) and the test polls
/// until the prompt flips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retitle_after_fifth_followup_changes_title() {
    let generator = Arc::new(InMemoryTitleGenerator {
        next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
        next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
    });
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(generator),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
    for i in 0..4 {
        e.follow_up(&id, &format!("f{i}"), false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
    }
    e.follow_up(&id, "fifth", false, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let mut landed = false;
    for _ in 0..40 {
        if e.get(&id).await.unwrap().prompt == "性能优化阶段二号" {
            landed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(landed, "5th-turn retitle never landed");
}

/// retitle_enabled=false suppresses the 5th-turn retitle task — the
/// prompt must not flip even if the generator would have returned
/// change=true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retitle_disabled_does_not_change_title() {
    let generator = Arc::new(InMemoryTitleGenerator {
        next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
        next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
    });
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(generator),
            retitle_enabled: Some(false),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    for i in 0..4 {
        e.follow_up(&id, &format!("f{i}"), false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
    }
    e.follow_up(&id, "fifth", false, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let prompt = e.get(&id).await.unwrap().prompt;
    assert_ne!(
        prompt, "性能优化阶段二号",
        "retitle must not run when retitle_enabled=false; got {prompt:?}"
    );
}

/// A user-pinned title (set via a set_title=true follow_up) must NOT be
/// overwritten by the periodic retitle, even when the cadence boundary is
/// hit and the generator would return a change. Guards P0#1 (title_pinned).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_title_survives_periodic_retitle() {
    let generator = Arc::new(InMemoryTitleGenerator {
        next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
        next_retitle: std::sync::Mutex::new(Some(Some("机器改的标题".to_string()))),
    });
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(generator),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap(); // turn 1
    wait_status(&e, &id, "done").await;
    // User manually renames the session → pins the title.
    e.follow_up(&id, "用户钉的标题", true, None, None, None)
        .await
        .unwrap(); // turn 2
    wait_status(&e, &id, "done").await;
    assert_eq!(e.get(&id).await.unwrap().prompt, "用户钉的标题");
    assert!(
        e.get(&id).await.unwrap().title_pinned,
        "set_title=true must set title_pinned"
    );
    // Drive to the 5-turn cadence boundary (turn 5) so a retitle fires.
    for i in 0..3 {
        e.follow_up(&id, &format!("f{i}"), false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
    }
    // Give any spawned retitle task time to (not) land.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let prompt = e.get(&id).await.unwrap().prompt;
    assert_eq!(
        prompt, "用户钉的标题",
        "pinned title must survive periodic retitle; got {prompt:?}"
    );
}

/// follow_up must NOT block on the retitle task even if the
/// underlying title generator were slow. The in-memory generator
/// is instant, but the key assertion is that the handler returns
/// immediately because the retitle is fire-and-forget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_returns_immediately_with_retitle_spawned() {
    let generator = Arc::new(InMemoryTitleGenerator {
        next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
        next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
    });
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            title_generator: Some(generator),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    for i in 0..4 {
        e.follow_up(&id, &format!("f{i}"), false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
    }
    let started = std::time::Instant::now();
    e.follow_up(&id, "fifth", false, None, None, None)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "follow_up must not block on the retitle task; took {elapsed:?}"
    );
}
