use rmcp::model::{ClientInfo, ListRootsResult, Root, RootsCapabilities};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ClientHandler, RoleClient};

use super::{Config, MempalMcpServer, setup_server};

/// Test client that advertises roots support and answers `list-roots` with a
/// fixed payload — or fails the request outright.
struct RootsFixtureClient {
    info: ClientInfo,
    roots: Vec<Root>,
    fail_roots: bool,
}

impl ClientHandler for RootsFixtureClient {
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::ErrorData> {
        if self.fail_roots {
            Err(rmcp::ErrorData::internal_error(
                "fixture roots request failure",
                None,
            ))
        } else {
            Ok(ListRootsResult::new(self.roots.clone()))
        }
    }
}

fn roots_client_info() -> ClientInfo {
    let mut info = ClientInfo::default();
    info.capabilities.roots = Some(RootsCapabilities::default());
    info
}

/// Serve the real MCP server over an in-memory duplex with a roots-capable
/// client. Both running services MUST be held by the caller: dropping either
/// cancels its task and closes the transport before the resolver's
/// `list-roots` round trip.
async fn serve_with_roots_client<H: ClientHandler>(
    server: &MempalMcpServer,
    client: H,
) -> (
    RunningService<RoleClient, H>,
    rmcp::service::RunningService<rmcp::RoleServer, MempalMcpServer>,
) {
    let (server_side, client_side) = tokio::io::duplex(1 << 16);
    let server_service = rmcp::serve_server(server.clone(), server_side);
    let server_handle = tokio::spawn(server_service);
    let client_service = rmcp::serve_client(client, client_side)
        .await
        .expect("client handshake");
    let server_service = server_handle
        .await
        .expect("server handshake")
        .expect("serve server");
    (client_service, server_service)
}

#[tokio::test]
async fn test_resolve_mcp_project_id_returns_none_for_empty_roots_client() {
    let (_tempdir, _db_path, server) = setup_server();
    let (_client, _server_service) = serve_with_roots_client(
        &server,
        RootsFixtureClient {
            info: roots_client_info(),
            roots: vec![],
            fail_roots: false,
        },
    )
    .await;

    let config = Config::parse("").expect("default config");
    let resolved = server.resolve_mcp_project_id(None, &config).await;
    assert!(
        matches!(&resolved, Ok(None)),
        "declared roots with empty list must resolve to Ok(None), got {resolved:?}"
    );
}

#[tokio::test]
async fn test_resolve_mcp_project_id_returns_none_for_invalid_root_uri() {
    let (_tempdir, _db_path, server) = setup_server();
    let (_client, _server_service) = serve_with_roots_client(
        &server,
        RootsFixtureClient {
            info: roots_client_info(),
            roots: vec![Root::new("not-a-project://opaque")],
            fail_roots: false,
        },
    )
    .await;

    let config = Config::parse("").expect("default config");
    let resolved = server.resolve_mcp_project_id(None, &config).await;
    assert!(
        matches!(&resolved, Ok(None)),
        "roots without a valid project URI must resolve to Ok(None), got {resolved:?}"
    );
}

#[tokio::test]
async fn test_resolve_mcp_project_id_returns_none_when_roots_request_fails() {
    let (_tempdir, _db_path, server) = setup_server();
    let (_client, _server_service) = serve_with_roots_client(
        &server,
        RootsFixtureClient {
            info: roots_client_info(),
            roots: vec![],
            fail_roots: true,
        },
    )
    .await;

    let config = Config::parse("").expect("default config");
    let resolved = server.resolve_mcp_project_id(None, &config).await;
    assert!(
        matches!(&resolved, Ok(None)),
        "failed roots request must resolve to Ok(None), got {resolved:?}"
    );
}

#[tokio::test]
async fn test_resolve_mcp_project_id_still_resolves_valid_root() {
    let (_tempdir, _db_path, server) = setup_server();
    let (_client, _server_service) = serve_with_roots_client(
        &server,
        RootsFixtureClient {
            info: roots_client_info(),
            roots: vec![Root::new("file:///tmp/mcp-roots-valid")],
            fail_roots: false,
        },
    )
    .await;

    let config = Config::parse("").expect("default config");
    let resolved = server.resolve_mcp_project_id(None, &config).await;
    assert!(
        matches!(&resolved, Ok(Some(project)) if project == "mcp-roots-valid"),
        "valid project root must still resolve, got {resolved:?}"
    );
}
