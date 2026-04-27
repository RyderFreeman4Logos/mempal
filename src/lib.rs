#![warn(clippy::all)]

pub mod aaak;
#[cfg(feature = "rest")]
pub mod api;
pub mod bootstrap_events;
pub mod context;
pub mod core;
pub mod cowork;
pub mod daemon;
pub mod daemon_bootstrap;
pub mod embed;
pub mod factcheck;
pub mod field_taxonomy;
pub mod hook;
pub mod hook_install;
pub mod hotpatch;
pub mod importance;
pub mod ingest;
pub mod integrations;
pub mod knowledge_anchor;
pub mod knowledge_distill;
pub mod knowledge_gate;
pub mod knowledge_lifecycle;
pub mod mcp;
pub mod observability;
pub mod search;
pub mod session_review;
