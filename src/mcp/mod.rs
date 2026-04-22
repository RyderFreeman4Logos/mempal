#![warn(clippy::all)]

mod server;
mod tools;

pub use server::MempalMcpServer;
pub use tools::{
    IngestRequest, IngestResponse, MAX_READ_DRAWERS_MAX_COUNT, MAX_READ_DRAWERS_REQUEST_IDS,
    ReadDrawerRequest, ReadDrawerResponse, ReadDrawersRequest, ReadDrawersResponse, SearchRequest,
    SearchResponse, StatusResponse,
};
