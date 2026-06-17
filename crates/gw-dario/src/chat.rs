use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream};
use crate::DarioConfig;

pub(crate) fn affinity_from_body(_body: &serde_json::Value) -> Option<String> { None }

pub(crate) async fn chat_via_sidecar(
    _cfg: &DarioConfig, _client: &reqwest::Client, _req: ChatRequest, _ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    Err(UpstreamError::new(UpstreamErrorKind::Other, "not implemented"))
}
