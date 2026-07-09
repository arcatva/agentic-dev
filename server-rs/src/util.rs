//! Small shared time helpers, deduplicated out of store/api/* (commit c0d15e3).
//!
//! `now_ms` returns milliseconds as `i64` (the store and usage cache compare against
//! `i64` timestamps); `now_secs` returns seconds as `u64` (token issue/verify takes `u64`).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in milliseconds since the Unix epoch.
#[inline]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Current wall-clock time in whole seconds since the Unix epoch.
#[inline]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
