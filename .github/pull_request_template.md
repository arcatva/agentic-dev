## What & why

<!-- One or two sentences: what this change does and why. Link any issue. -->

## Verification

<!-- How you know it works. This repo's merge floor is that it compiles. -->

- [ ] `cd server-rs && cargo build` passes
- [ ] `cd server-rs && cargo test` result noted (green, or known-flaky called out)
- [ ] `cd server-rs && cargo fmt --check` clean and `cargo clippy` reviewed
- [ ] Behavior-preserving? If not, the API surface the Android client depends on is unchanged or the change is intentional and noted

## Notes for review

<!-- Anything the reviewer should focus on, risks, or follow-ups. -->
