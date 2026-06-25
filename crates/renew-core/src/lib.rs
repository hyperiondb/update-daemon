//! Core engine for renew: a self-healing deployment sync that
//! reconciles running docker-compose state against a git repo on a cron-like
//! schedule, gated by peer health quorum. All effectful operations (process
//! exec, HTTP health probes, the clock) sit behind traits so the engine is
//! exercised end-to-end by fakes in unit tests.

pub mod clock;
pub mod config;
pub mod daemon;
pub mod docker;
pub mod envhash;
pub mod error;
pub mod git;
pub mod health;
pub mod runner;
pub mod schedule;
pub mod sync;

#[cfg(test)]
mod sync_tests;

pub use clock::{Clock, SystemClock};
pub use config::{ResolvedConfig, ResolvedService};
pub use daemon::{resolve_config_path, Daemon};
pub use error::{Error, Result};
pub use health::HealthChecker;
pub use runner::{CommandRunner, SystemRunner};
pub use schedule::Schedule;
pub use sync::{SyncEngine, SyncOptions, SyncOutcome, SyncStatus};
