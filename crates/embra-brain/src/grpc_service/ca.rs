//! `/ca`: the certificate authorities this machine trusts beside the public
//! ones, listed, imported and removed (`crate::ca`). Answered before a
//! session as in one (`update.rs`, `SetupCommands`): the provider gateway's
//! authority is imported ahead of the provider's setup.

use tokio::sync::mpsc;
use tonic::Status;

use embra_common::proto::brain::{conversation_response, ConversationResponse, SystemMessage, SystemMessageType};

/// Runs the command on the directories in force and the process's roots,
/// and answers on the client's stream. It needs no session, no database
/// and no configuration.
pub(super) async fn handle_ca_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let reply = crate::ca::answer(args, &crate::ca::CaEnv::in_force()).await;
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
