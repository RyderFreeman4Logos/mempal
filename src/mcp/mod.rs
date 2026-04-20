#![warn(clippy::all)]

mod server;
mod tools;

pub use server::MempalMcpServer;
pub use tools::{IngestRequest, IngestResponse, SearchRequest, SearchResponse, StatusResponse};
