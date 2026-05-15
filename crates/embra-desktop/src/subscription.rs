//! gRPC subscription bridge.
//!
//! Long-lived async worker: connects `BrainClient` to `embra-apid`, opens
//! the bidirectional Converse stream, and forwards each `ConsoleEvent`
//! through iced's stream channel as a `Message::GrpcEvent`. The handshake
//! emits a single `Message::GrpcConnected(ConversationRequest sender)`
//! up front so `update()` can later push `UserMessage` / `SlashCommand`.
//!
//! On connect failure or stream EOF the worker re-tries with a 2s back-
//! off and re-emits `GrpcConnected` after each successful re-handshake.
//!
//! Wired into iced via `Subscription::run_with(addr, build_stream)` —
//! iced 0.14 requires `builder: fn(&D) -> S` (non-capturing function
//! pointer), so we hash on the address String, clone it inside the
//! function, and let the inner async closure own its copy.

use embra_common::proto::apid::ConversationRequest;
use embra_console_core::grpc::{BrainClient, ConsoleEvent};
use iced::event::{self, Event};
use iced::futures::SinkExt;
use iced::keyboard::{self, key::Named, Key};
use iced::Subscription;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::menu::{MenuPanel, NavDir};
use crate::Message;

/// Global keyboard shortcuts. Fires regardless of focus, so behaviors
/// must be context-aware in `update()` (e.g. ArrowUp navigates a
/// selector when one is active and scrolls history otherwise). The
/// text_input widget keeps its own character handling on top.
pub fn keyboard() -> Subscription<Message> {
    event::listen_with(|event, _status, _window_id| {
        let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
            return None;
        };
        match key.as_ref() {
            Key::Named(Named::ArrowUp) => Some(Message::ArrowUp),
            Key::Named(Named::ArrowDown) => Some(Message::ArrowDown),
            Key::Named(Named::ArrowLeft) => Some(Message::MenuNavigate(NavDir::Left)),
            Key::Named(Named::ArrowRight) => Some(Message::MenuNavigate(NavDir::Right)),
            Key::Named(Named::PageUp) => Some(Message::PageUp),
            Key::Named(Named::PageDown) => Some(Message::PageDown),
            Key::Named(Named::Enter) if modifiers.control() => Some(Message::EditorSubmit),
            Key::Named(Named::Enter) => Some(Message::MenuActivate),
            Key::Named(Named::Escape) => Some(Message::MenuClose),
            Key::Character("f") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::File)),
            Key::Character("v") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::View)),
            Key::Character("p") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::Provider)),
            Key::Character("s") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::Settings)),
            Key::Character("t") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::Setup)),
            Key::Character("h") if modifiers.alt() => Some(Message::MenuOpen(MenuPanel::Help)),
            Key::Character("c") if modifiers.control() => Some(Message::Quit),
            Key::Character("d") if modifiers.control() => Some(Message::Quit),
            _ => None,
        }
    })
}

pub fn grpc(apid_addr: String) -> Subscription<Message> {
    // `Subscription::run_with` wants a non-capturing `fn(&D) -> S` pointer.
    // `impl Trait` on a function item doesn't coerce to a fn pointer, so
    // we box the stream into a concrete return type. The address String
    // is re-cloned inside the closure so the inner async block owns it.
    Subscription::run_with(apid_addr, build_grpc_stream)
}

type BoxStream = std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>>;

// `&String` (not `&str`) is load-bearing here: `Subscription::run_with`
// requires `builder: fn(&D) -> S` where `D` is the data type — we pass
// `apid_addr: String`, so the parameter type must be `&String` to make
// the fn pointer signature `for<'a> fn(&'a String) -> BoxStream` match.
// `&str` would change the fn pointer type and cause a coercion failure.
#[allow(clippy::ptr_arg)]
fn build_grpc_stream(apid_addr: &String) -> BoxStream {
    let addr = apid_addr.clone();
    Box::pin(iced::stream::channel(64, move |mut output| {
        let addr = addr.clone();
        async move {
            loop {
                match connect_once(&addr, &mut output).await {
                    Ok(()) => {
                        tracing::warn!("grpc stream ended cleanly; reconnecting in 2s");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "grpc connect/run failed; reconnecting in 2s");
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }))
}

async fn connect_once(
    apid_addr: &str,
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let mut client = BrainClient::connect(apid_addr).await?;
    let (in_tx, mut rx): (mpsc::Sender<ConversationRequest>, mpsc::Receiver<ConsoleEvent>) =
        client.open_conversation("").await?;
    let _ = output.send(Message::GrpcConnected(in_tx)).await;

    while let Some(event) = rx.recv().await {
        // Once the iced runtime has stopped consuming our messages the
        // send fails — return so the outer loop doesn't spin forever
        // pushing into a dead channel.
        if output.send(Message::GrpcEvent(event)).await.is_err() {
            anyhow::bail!("iced output channel closed");
        }
    }
    Ok(())
}
