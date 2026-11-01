//! embra-console — TUI client for serial console.

/// A line of the console's own: on stdout, where the terminal is, and in
/// the log when one is installed (`install_log`). The printed form keeps
/// its `[embra-console]` or `[TUI]` prefix; the log line's target names
/// the module instead.
macro_rules! say {
    ($prefix:literal, $($arg:tt)*) => {{
        println!("{} {}", $prefix, format_args!($($arg)*));
        tracing::info!($($arg)*);
    }};
}

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;

/// The log's filter when `RUST_LOG` is unset or does not parse: INFO for
/// every target, as embra-trustd and embra-apid have it, so a dependency's
/// warnings reach the log too.
const DEFAULT_LOG_DIRECTIVE: &str = "info";

/// The console's log goes to stderr, when stderr is not a terminal.
///
/// On the serial line embrad gives the console the boot's stdio and sends
/// its stderr to `/embra/ephemeral/embra-console.log`. The file was 0 bytes
/// on every boot (Embra, 2026-11-01): nothing here wrote to stderr — the
/// console's lines were `println!` on the serial terminal, painted over by
/// the next console's screen within the restart backoff, and its `tracing`
/// calls had no subscriber. On the web PTY all three streams are the pane
/// the operator sees, so nothing is installed there and the `tracing` calls
/// stay no-ops: the pane says each line once. ANSI off, the writer is a
/// file.
fn install_log() {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(log_filter_from(std::env::var("RUST_LOG").ok().as_deref()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// `RUST_LOG` when it is set and parses, otherwise the default.
fn log_filter_from(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    rust_log
        .and_then(|spec| tracing_subscriber::EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_DIRECTIVE))
}

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

/// Say why, and exit. The supervisor starts the console again. The reason
/// goes to stdout, where the terminal is, and as an ERROR line with the
/// code to the log when one is installed.
fn fail(code: i32, why: std::fmt::Arguments) -> ! {
    use std::io::Write;
    println!("[embra-console] {why}");
    let _ = std::io::stdout().flush();
    tracing::error!(code, "{why}");
    std::process::exit(code)
}

#[tokio::main]
async fn main() {
    install_log();
    say!("[embra-console]", "starting");

    // The flags come through the one parser of the services
    // (`embra_common::args`, #41); a flag without its value stops the console.
    let flags = embra_common::args::Flags::parse(std::env::args().skip(1), &["--apid-addr", "--device", "--columns", "--rows"])
        .unwrap_or_else(|e| panic!("{e}"));
    let apid_addr = flags.value("--apid-addr").unwrap_or("http://127.0.0.1:50000").to_string();
    let device = flags.value("--device").map(str::to_string);
    let columns: Option<u16> = flags.value("--columns").and_then(|v| v.parse().ok());
    let rows: Option<u16> = flags.value("--rows").and_then(|v| v.parse().ok());

    say!("[embra-console]", "connecting to {}", apid_addr);
    let client = match BrainClient::connect(&apid_addr).await {
        Ok(c) => {
            say!("[embra-console]", "connected");
            c
        }
        Err(e) => {
            say!("[embra-console]", "connect failed: {}, retrying...", e);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match BrainClient::connect(&apid_addr).await {
                Ok(c) => c,
                Err(e2) => fail(exit_code::NO_GATEWAY, format_args!("FATAL: {e2}")),
            }
        }
    };

    say!("[embra-console]", "launching TUI...");
    match terminal::run(client, device, columns, rows).await {
        Ok(None) => say!("[embra-console]", "exited"),
        Ok(Some(grpc_client::StreamEnd::Closed)) => {
            fail(exit_code::CONVERSATION_ENDED, format_args!("the gateway ended the conversation stream"))
        }
        Ok(Some(grpc_client::StreamEnd::Failed(why))) => {
            fail(exit_code::CONVERSATION_FAILED, format_args!("the conversation stream failed: {why}"))
        }
        Err(e) => fail(exit_code::TUI_FAILED, format_args!("TUI error: {e}")),
    }
}
