//! `/network`: this machine's network cards, their addresses and its name,
//! shown and set (`crate::network`). Answered before a session as in one
//! (`update.rs`, `SetupCommands`): a machine on a network without DHCP is
//! given its address from its own display or its serial line, ahead of
//! everything that needs the network.

use tokio::sync::mpsc;
use tonic::Status;

use embra_common::proto::brain::{conversation_response, ConversationResponse, SystemMessage, SystemMessageType};

/// Runs the command on the machine's cards and settings and answers on the
/// client's stream. It needs no session, no database and no configuration.
pub(super) async fn handle_network_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let reply = crate::network::answer(args, &crate::network::NetEnv::in_force());
    let kind = if reply.refused { SystemMessageType::Error } else { SystemMessageType::Info };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: reply.text,
                msg_type: kind as i32,
            })),
        }))
        .await;
}
