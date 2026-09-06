//! One process-wide lock for tests that mutate environment variables.
//!
//! `cargo test` runs a binary's tests on parallel threads and the environment
//! is process-global, so two tests touching it race and the loser reads the
//! other's value. Per-module locks do not compose: `HOME`, `RESEED_MESSAGES`
//! and `STATUSLINE_RESET_LINE` all live in the same environment.

use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

/// Hold for the whole span in which the environment is mutated AND read.
/// Poisoning is ignored: a panicking test must not cascade into the rest.
pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}
