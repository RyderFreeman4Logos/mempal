#![warn(clippy::all)]

mod ingest_payload;
mod logging;
mod resource_usage;
mod server;
mod stale_daemon;
mod timeline;
mod tools;

pub use resource_usage::{
    MemoryPressureDto, ProcessResourceUsageDto, ProfileDbAdmissionDto, ProfileDbHolderDto,
    ResourceCounterDto, ResourceUsageDto, SqliteResourceUsageDto,
};
pub use server::{IngestDrainWorkerHandle, MempalMcpServer, daemon_ingest_ipc_available_for_path};
pub use timeline::{TimelineRequest, TimelineResponse};
pub use tools::{
    IngestControls, IngestOperationState, IngestRequest, IngestResponse,
    MAX_READ_DRAWERS_MAX_COUNT, MAX_READ_DRAWERS_REQUEST_IDS, OperationStatusRequest,
    PinnedFactDto, PinnedFactProjectCount, PinnedFactsRequest, PinnedFactsResponse,
    ReadDrawerRequest, ReadDrawerResponse, ReadDrawersRequest, ReadDrawersResponse,
    RetrievalScopeRequest, RollbackRequest, RollbackResponse, RouteDecisionDto, SearchRequest,
    SearchResponse, SearchResultDto, StatusDetail, StatusRequest, StatusResponse, StatusScope,
};
