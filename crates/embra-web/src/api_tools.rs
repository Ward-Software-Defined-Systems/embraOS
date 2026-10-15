//! `GET /api/tools?limit=&before=&session=` — the tool-call record read
//! back (apid's `ListToolCalls`, the brain's `tools.turn_trace`), newest
//! first, for the Tools tab's backfill after a restart and its paging.
//! `before` is the exact `started_at_rfc` of the oldest call shown.
//!
//! JSON: `{"calls": [...]}`, each call `from_record: true` (2 KiB previews,
//! no byte totals). A brain away answers 503 `{"calls": [], "error": ..}`.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use embra_common::proto::apid::ListToolCallsRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use serde::Deserialize;
use serde_json::{Value, json};
use tonic::transport::Channel;

use crate::state::AppState;
use crate::tool_bridge::{ToolCall, record_to_call};

/// The most calls one page may ask for; the brain clamps to the same.
pub const PAGE_MAX: u32 = 200;

#[derive(Debug, Default, Deserialize)]
pub struct ToolsQuery {
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub session: String,
}

pub async fn api_tools_list(
    State(st): State<AppState>,
    Query(q): Query<ToolsQuery>,
) -> (StatusCode, Json<Value>) {
    match fetch_calls(&st.apid_addr, &q).await {
        Ok(calls) => (StatusCode::OK, Json(listing_json(calls))),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "calls": [], "error": e })),
        ),
    }
}

async fn fetch_calls(apid_addr: &str, q: &ToolsQuery) -> Result<Vec<ToolCall>, String> {
    let endpoint = Channel::from_shared(apid_addr.to_string())
        .map_err(|e| format!("invalid apid endpoint: {e}"))?;
    let mut client = EmbraApiClient::new(endpoint.connect_lazy())
        .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES);
    let payload = brain::ListToolCallsRequest {
        limit: q.limit.min(PAGE_MAX),
        before: q.before.clone(),
        session: q.session.clone(),
    }
    .encode_to_vec();
    let response = client
        .list_tool_calls(ListToolCallsRequest { payload })
        .await
        .map_err(|e| format!("ListToolCalls RPC failed: {e}"))?;
    let bytes = response.into_inner().payload;
    let brain_resp = brain::ListToolCallsResponse::decode(bytes.as_slice())
        .map_err(|e| format!("decode brain response: {e}"))?;
    Ok(brain_resp.records.into_iter().map(record_to_call).collect())
}

/// The page's JSON: the calls as `/ws/tools` shapes them, each marked as
/// read back from the record.
fn listing_json(calls: Vec<ToolCall>) -> Value {
    json!({ "calls": calls })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tools_listing_json_carries_from_record_and_the_cursor() {
        let call = record_to_call(brain::ToolCallRecord {
            seq: 3,
            tool_use_id: "toolu_01".into(),
            name: "remember".into(),
            session: "main".into(),
            origin: "turn".into(),
            started_at_ms: 1_792_072_931_250,
            elapsed_ms: 40,
            is_error: false,
            stopped: false,
            input_json: "{\"content\":\"x\"}".into(),
            input_bytes: 0,
            result: "saved".into(),
            result_bytes: 0,
            dropped: 0,
            job_id: String::new(),
            from_record: true,
            started_at_rfc: "2026-10-15T14:02:11.250+00:00".into(),
        });
        let page = listing_json(vec![call]);
        let first = &page["calls"][0];
        assert_eq!(first["from_record"], true);
        assert_eq!(first["started_at_rfc"], "2026-10-15T14:02:11.250+00:00");
        assert_eq!(first["input_bytes"], 0, "the record keeps no byte totals");
        assert_eq!(first["result"], "saved");
        assert_eq!(page["calls"].as_array().map(|a| a.len()), Some(1));
    }
}
