#![warn(clippy::all)]

mod server;
mod tools;

pub use server::MempalMcpServer;
pub use tools::{
    IngestRequest, IngestResponse, ReadDrawerRequest, ReadDrawerResponse, SearchRequest,
    SearchResponse, StatusResponse,
};
