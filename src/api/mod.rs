#![warn(clippy::all)]

#[cfg(feature = "rest")]
mod durable;
#[cfg(feature = "rest")]
mod handlers;
#[cfg(feature = "rest")]
mod hermes_compat;
#[cfg(feature = "rest")]
mod mcp;
#[cfg(feature = "rest")]
mod resource_status;
#[cfg(feature = "rest")]
mod search;
#[cfg(feature = "rest")]
mod state;

#[cfg(feature = "rest")]
pub use handlers::{
    DEFAULT_REST_ADDR, MAX_REST_INGEST_BODY_BYTES, router, router_with_mcp, router_with_mcp_at,
    serve, serve_with_mcp, serve_with_optional_mcp, serve_with_shutdown,
};
#[cfg(feature = "rest")]
pub use state::ApiState;
