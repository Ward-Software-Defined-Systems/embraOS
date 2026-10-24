//! `POST /api/stop` — out-of-band operator interrupt for a stuck turn.
//!
//! The chat-mobile ■ button calls this instead of sending a `/stop`
//! slash over `/ws/chat`: anything on the Converse path parks behind the
//! running turn (the brain's loop awaits each turn inline), so the stop
//! must travel as a separate unary. Same fresh-channel pattern as
//! `/api/sessions`.
//!
//! Returns JSON `{ "was_in_turn": bool }`, or `{ "was_in_turn": false,
//! "error": "..." }` on transport failure.

use axum::Json;
use axum::extract::State;
use embra_common::proto::apid::StopTurnRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use serde_json::{Value, json};
use tonic::transport::Channel;

use crate::state::AppState;

pub async fn api_stop(State(st): State<AppState>) -> Json<Value> {
    match send_stop(&st.apid_addr).await {
        Ok(was_in_turn) => Json(json!({ "was_in_turn": was_in_turn })),
        Err(e) => Json(json!({ "was_in_turn": false, "error": e })),
    }
}

async fn send_stop(apid_addr: &str) -> Result<bool, String> {
    let endpoint = Channel::from_shared(apid_addr.to_string())
        .map_err(|e| format!("invalid apid endpoint: {e}"))?;
    let channel = endpoint.connect_lazy();
    let mut client = EmbraApiClient::new(channel);

    let response = client
        .stop_turn(StopTurnRequest {})
        .await
        .map_err(|e| format!("StopTurn RPC failed: {e}"))?;

    let bytes = response.into_inner().payload;
    let brain_resp = brain::StopTurnResponse::decode(bytes.as_slice())
        .map_err(|e| format!("decode brain response: {e}"))?;
    Ok(brain_resp.was_in_turn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stop is answered either way: with no apid behind the door it says
    /// so in `error` and `was_in_turn` is false, so the client shows a
    /// failed stop and never a success (#54).
    #[tokio::test]
    async fn a_stop_with_no_apid_behind_the_door_is_answered_as_a_failure() {
        let (st, _input) = AppState::stub();
        let Json(answer) = api_stop(State(st)).await;
        assert_eq!(answer["was_in_turn"], false);
        let err = answer["error"].as_str().expect("an error");
        assert!(err.starts_with("StopTurn RPC failed: "), "{err}");
    }
}
