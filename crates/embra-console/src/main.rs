//! embra-console — TUI client for serial console.

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;

/// How the console ends when it cannot do its work.
///
/// It is a supervised child on both transports — embra-web's session
/// manager on the PTY, embrad on the serial line — and both restart what
/// EXITS. A failure that ended in a sleep was neither restarted nor seen:
/// the process stayed alive, and its one line of explanation went to a
/// terminal that may have had nobody on it. The web console's empty pane of
/// 2026-09-27 was exactly that.
mod exit_code {
    /// The conversation could not be opened, or the screen could not be set
    /// up or drawn.
    pub const TUI_FAILED: i32 = 1;
    /// embra-apid could not be reached.
    pub const NO_GATEWAY: i32 = 2;
    /// The gateway ended the conversation stream: apid or the brain
    /// restarted, or the connection was reset (Embra #21; the console
    /// exited 0 for this, with the cause nowhere). embra-web names the
    /// code in its exit line (`pty_bridge::describe_exit`).
    pub const CONVERSATION_ENDED: i32 = 3;
    /// The conversation stream failed with a status.
    pub const CONVERSATION_FAILED: i32 = 4;
}

/// Say why, and exit. The supervisor starts the console again.
fn fail(code: i32, why: std::fmt::Arguments) -> ! {
    use std::io::Write;
    println!("[embra-console] {why}");
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

#[tokio::main]
async fn main() {
    println!("[embra-console] starting");

    // The flags come through the one parser of the services
    // (`embra_common::args`, #41); a flag without its value stops the console.
    let flags = embra_common::args::Flags::parse(std::env::args().skip(1), &["--apid-addr", "--device", "--columns", "--rows"])
        .unwrap_or_else(|e| panic!("{e}"));
    let apid_addr = flags.value("--apid-addr").unwrap_or("http://127.0.0.1:50000").to_string();
    let device = flags.value("--device").map(str::to_string);
    let columns: Option<u16> = flags.value("--columns").and_then(|v| v.parse().ok());
    let rows: Option<u16> = flags.value("--rows").and_then(|v| v.parse().ok());

    println!("[embra-console] connecting to {}", apid_addr);
    let client = match BrainClient::connect(&apid_addr).await {
        Ok(c) => {
            println!("[embra-console] connected");
            c
        }
        Err(e) => {
            println!("[embra-console] connect failed: {}, retrying...", e);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match BrainClient::connect(&apid_addr).await {
                Ok(c) => c,
                Err(e2) => fail(exit_code::NO_GATEWAY, format_args!("FATAL: {e2}")),
            }
        }
    };

    println!("[embra-console] launching TUI...");
    match terminal::run(client, device, columns, rows).await {
        Ok(None) => println!("[embra-console] exited"),
        Ok(Some(grpc_client::StreamEnd::Closed)) => {
            fail(exit_code::CONVERSATION_ENDED, format_args!("the gateway ended the conversation stream"))
        }
        Ok(Some(grpc_client::StreamEnd::Failed(why))) => {
            fail(exit_code::CONVERSATION_FAILED, format_args!("the conversation stream failed: {why}"))
        }
        Err(e) => fail(exit_code::TUI_FAILED, format_args!("TUI error: {e}")),
    }
}
