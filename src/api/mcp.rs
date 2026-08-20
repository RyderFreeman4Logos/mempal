use std::sync::Arc;

use crate::mcp::MempalMcpServer;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::never::NeverSessionManager,
};

pub(super) fn service(
    server: MempalMcpServer,
) -> StreamableHttpService<MempalMcpServer, NeverSessionManager> {
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    )
}
