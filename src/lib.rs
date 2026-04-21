#![warn(clippy::all)]

pub mod aaak;
#[cfg(feature = "rest")]
pub mod api;
pub mod bootstrap_events;
pub mod core;
pub mod cowork;
pub mod daemon;
pub mod daemon_bootstrap;
pub mod embed;
pub mod factcheck;
pub mod hook;
pub mod hook_install;
pub mod hotpatch;
pub mod ingest;
pub mod mcp;
pub mod search;
pub mod session_review;
