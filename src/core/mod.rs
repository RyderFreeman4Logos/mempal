#![warn(clippy::all)]

pub mod anchor;
pub mod async_db;
pub mod case_skill;
pub mod compaction;
pub mod config;
pub mod db;
pub mod db_admission;
mod db_admission_budget;
#[cfg(all(test, target_os = "linux"))]
mod db_admission_crash_tests;
mod db_admission_diagnostics;
#[cfg(test)]
mod db_admission_fault_injection;
mod db_admission_lease;
mod db_admission_paths;
mod db_admission_release;
#[cfg(test)]
mod db_admission_sidecar_tests;
mod db_admission_state;
pub(crate) mod db_connection;
pub(crate) mod deadline;
pub use async_db::{AsyncDb, QueryOnlyAsyncDb};
pub mod decay;
pub mod design_insights;
mod evidence_config;
pub mod foresight;
pub mod hot_reload;
pub mod patterns;
pub mod phase3;
pub mod priming;
pub(crate) mod process_identity;
pub mod project;
pub mod protocol;
pub mod queue;
mod queue_connection_admission;
mod queue_queries;
pub mod reindex;
pub mod remote_calls;
pub mod skills;
pub mod sqlite_retry;
pub mod strata;
pub mod timeline;
pub mod types;
pub mod utils;
