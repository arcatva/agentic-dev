//! API contract guard.
//!
//! Locks the serialized JSON **key set** of the core response types the Android client
//! (`agentic-dev-android`'s `data/net/Models.kt`) deserializes. If a field is added, removed, or
//! renamed on the wire, this test fails — forcing the change to be a *conscious* contract update
//! that must be mirrored in the client. This is the server half of the cross-repo contract guard
//! (spec §4.10 / C1); the client half deserializes the same fixtures.
//!
//! Regenerate fixtures after an intentional contract change:
//!   UPDATE_CONTRACT=1 cargo test --test contract
//! then mirror the change in the Android client and commit the updated fixtures.

use agentic_dev_server::engine::store::{CreateInput, Store};
use std::collections::BTreeSet;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("agentic-contract-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

/// Top-level object keys as a set (BTreeSet iterates in sorted order, so the fixture is stable).
fn extract_keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Compare the top-level key set of `value` against the committed fixture `<name>.keys.json`.
/// In UPDATE_CONTRACT mode, (re)writes the fixture instead of asserting.
fn assert_contract(name: &str, value: &serde_json::Value) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/contract");
    let path = format!("{dir}/{name}.keys.json");
    let actual = extract_keys(value);

    if std::env::var("UPDATE_CONTRACT").is_ok() {
        std::fs::create_dir_all(dir).expect("create contract dir");
        // A BTreeSet serializes as a sorted JSON array, so fixtures are deterministic.
        let body = serde_json::to_string_pretty(&actual).expect("serialize keys");
        std::fs::write(&path, format!("{body}\n")).expect("write fixture");
        return;
    }

    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing contract fixture {path} — run `UPDATE_CONTRACT=1 cargo test --test contract`")
    });
    let expected: BTreeSet<String> = serde_json::from_str(&raw).expect("parse fixture");
    let added: Vec<&String> = actual.difference(&expected).collect();
    let removed: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "{name} wire contract drift — added: {added:?}, removed: {removed:?}.\n\
         If intentional: mirror it in the Android client (data/net/Models.kt) and regenerate with \
         `UPDATE_CONTRACT=1 cargo test --test contract`."
    );
}

#[tokio::test]
async fn session_wire_contract_is_stable() {
    let dir = tmp_dir("session");
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "contract-1".into(),
            prompt: "hi".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = store.get("contract-1").await.unwrap().unwrap();
    let value = serde_json::to_value(&session).expect("serialize Session");
    assert_contract("session", &value);
}
