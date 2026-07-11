// engine::session — grouped by responsibility (behavior-preserving regroup).
pub mod auto_resume;
pub mod groups;
pub mod lifecycle;
pub mod recover;
pub mod resume_gate;
pub mod status;
pub mod transition;
pub mod watchdog;

// Engine method clusters extracted from the former monolithic mod.rs.
mod admin;
mod native_sync;
mod read;
mod runtime;
mod submit;
