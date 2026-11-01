//! The console binary, started the way its supervisors start it.
//!
//! When it cannot do its work it has to say why and EXIT. embra-web's
//! session manager and embrad both restart what exits; a console that
//! sleeps instead is alive to them, so it is never restarted, and the
//! operator is left with a terminal that shows nothing.
//!
//! Each test gives the console a deadline. Before the change it slept for
//! an hour at a time, so these did not fail — they did not end.

use std::os::unix::process::CommandExt;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use embra_common::proto::apid::embra_api_server::{EmbraApi, EmbraApiServer};
use embra_common::proto::apid::*;
use embra_common::proto::common;
use futures::Stream;
use tonic::{Request, Response, Status, Streaming};

const CONSOLE: &str = env!("CARGO_BIN_EXE_embra-console");
const DEADLINE: Duration = Duration::from_secs(30);

struct Ended {
    code: Option<i32>,
    /// Everything it wrote: stdout then stderr on pipes, the PTY's output
    /// on a PTY.
    said: String,
    /// Its stderr alone, on pipes: where its log goes when stderr is not a
    /// terminal. Empty on a PTY, where stderr is the terminal.
    logged: String,
}

/// Run the console against `apid_addr` until it exits. Its standard
/// streams are pipes and it has no controlling terminal: crossterm falls
/// back to `/dev/tty` when stdin is not a terminal, and the terminal
/// `cargo test` was started from must not become the console's.
fn run_console(apid_addr: &str) -> Ended {
    let mut cmd = Command::new(CONSOLE);
    cmd.args(["--apid-addr", apid_addr])
        .env("EMBRA_WEB_PTY", "1")
        // The log's default filter is under test, not this shell's.
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe; it detaches the child from the
    // test runner's terminal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("start the console");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait for the console") {
            let out = child.wait_with_output().expect("the console's output");
            let logged = String::from_utf8_lossy(&out.stderr).into_owned();
            let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
            said.push_str(&logged);
            return Ended { code: status.code(), said, logged };
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let out = child.wait_with_output().expect("the console's output");
            panic!(
                "the console was still running after {DEADLINE:?}; it said:\n{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The console on a PTY of 80x24 against `apid_addr`, until it exits: the
/// screen comes up there, so the event loop runs and an ended conversation
/// is seen by it (Embra #21). The master side is read until the exit.
fn run_console_on_a_pty(apid_addr: &str) -> Ended {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let (mut master, mut slave) = (0, 0);
    let size = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 800, ws_ypixel: 480 };
    // SAFETY: openpty fills the two descriptors; the winsize is a plain C
    // struct that outlives the call.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut cmd = Command::new(CONSOLE);
    cmd.args(["--apid-addr", apid_addr])
        .env("EMBRA_WEB_PTY", "1")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: between fork and exec, only async-signal-safe calls: the child
    // becomes a session leader with the PTY as its terminal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("start the console on a pty");
    drop(cmd);
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let mut read_some = |seen: &mut Vec<u8>| -> bool {
        let mut fds = [libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        // SAFETY: one valid pollfd, a bounded wait.
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) } <= 0 {
            return false;
        }
        // SAFETY: reads into a buffer of the stated length.
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            seen.extend_from_slice(&buf[..n as usize]);
            return true;
        }
        false
    };
    loop {
        read_some(&mut seen);
        if let Some(status) = child.try_wait().expect("wait for the console") {
            // What the console wrote on its way out, still in the PTY.
            for _ in 0..20 {
                if !read_some(&mut seen) {
                    break;
                }
            }
            return Ended { code: status.code(), said: String::from_utf8_lossy(&seen).into_owned(), logged: String::new() };
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the console was still running after {DEADLINE:?}; it wrote:\n{}",
                String::from_utf8_lossy(&seen).escape_debug()
            );
        }
    }
}

/// A port on this host that nothing listens on.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address").port()
}

#[test]
fn an_unreachable_gateway_is_an_exit_not_a_sleep() {
    let ended = run_console(&format!("http://127.0.0.1:{}", closed_port()));
    assert_eq!(ended.code, Some(2), "it said:\n{}", ended.said);
    // It tried twice, and says what the second attempt ran into.
    assert!(ended.said.contains("connect failed"), "{}", ended.said);
    assert!(ended.said.contains("[embra-console] FATAL: "), "{}", ended.said);
}

/// embra-apid as far as the console needs it to get to its screen: the
/// conversation opens and stays open, and says nothing — or ends at once
/// (`conversation_ends`), the gateway closing it as a restarted apid does.
struct Gateway {
    conversation_ends: bool,
}

fn not_here() -> Status {
    Status::unimplemented("stand-in gateway")
}

#[tonic::async_trait]
impl EmbraApi for Gateway {
    type ConverseStream = Pin<Box<dyn Stream<Item = Result<ConversationResponse, Status>> + Send>>;

    async fn converse(
        &self,
        _request: Request<Streaming<ConversationRequest>>,
    ) -> Result<Response<Self::ConverseStream>, Status> {
        if self.conversation_ends {
            return Ok(Response::new(Box::pin(futures::stream::empty())));
        }
        Ok(Response::new(Box::pin(futures::stream::pending())))
    }
    async fn list_sessions(&self, _: Request<ListSessionsRequest>) -> Result<Response<ListSessionsResponse>, Status> {
        Err(not_here())
    }
    async fn create_session(&self, _: Request<CreateSessionRequest>) -> Result<Response<CreateSessionResponse>, Status> {
        Err(not_here())
    }
    async fn switch_session(&self, _: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        Err(not_here())
    }
    async fn close_session(&self, _: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        Err(not_here())
    }
    async fn get_expression(&self, _: Request<GetExpressionRequest>) -> Result<Response<ExpressionState>, Status> {
        Err(not_here())
    }
    type WatchActivityStream = Pin<Box<dyn Stream<Item = Result<ActivityFrame, Status>> + Send>>;
    async fn watch_activity(&self, _: Request<WatchActivityRequest>) -> Result<Response<Self::WatchActivityStream>, Status> {
        Err(not_here())
    }
    type WatchToolCallsStream = Pin<Box<dyn Stream<Item = Result<ToolCallRecord, Status>> + Send>>;
    async fn watch_tool_calls(&self, _: Request<WatchToolCallsRequest>) -> Result<Response<Self::WatchToolCallsStream>, Status> {
        Err(not_here())
    }
    async fn list_tool_calls(&self, _: Request<ListToolCallsRequest>) -> Result<Response<ListToolCallsResponse>, Status> {
        Err(not_here())
    }
    async fn stop_turn(&self, _: Request<StopTurnRequest>) -> Result<Response<StopTurnResponse>, Status> {
        Err(not_here())
    }
    async fn put_media(&self, _: Request<PutMediaRequest>) -> Result<Response<PutMediaResponse>, Status> {
        Err(not_here())
    }
    async fn get_media(&self, _: Request<GetMediaRequest>) -> Result<Response<GetMediaResponse>, Status> {
        Err(not_here())
    }
    async fn get_file(&self, _: Request<GetFileRequest>) -> Result<Response<GetFileResponse>, Status> {
        Err(not_here())
    }
    async fn verify_soul(&self, _: Request<VerifySoulRequest>) -> Result<Response<VerifySoulResponse>, Status> {
        Err(not_here())
    }
    async fn get_soul_status(&self, _: Request<GetSoulStatusRequest>) -> Result<Response<GetSoulStatusResponse>, Status> {
        Err(not_here())
    }
    async fn system_health(&self, _: Request<SystemHealthRequest>) -> Result<Response<SystemHealthResponse>, Status> {
        Err(not_here())
    }
    async fn list_services(&self, _: Request<ListServicesRequest>) -> Result<Response<ListServicesResponse>, Status> {
        Err(not_here())
    }
    async fn get_version(&self, _: Request<GetVersionRequest>) -> Result<Response<GetVersionResponse>, Status> {
        Err(not_here())
    }
    async fn health_check(
        &self,
        _: Request<common::HealthCheckRequest>,
    ) -> Result<Response<common::HealthCheckResponse>, Status> {
        Err(not_here())
    }
}

/// The stand-in gateway on a port of its own, for as long as the test runs.
fn gateway() -> (u16, tokio::runtime::Runtime) {
    gateway_with(false)
}

fn gateway_with(conversation_ends: bool) -> (u16, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind the stand-in gateway");
    let port = listener.local_addr().expect("address").port();
    runtime.spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = tonic::transport::Server::builder()
            .add_service(EmbraApiServer::new(Gateway { conversation_ends }))
            .serve_with_incoming(incoming)
            .await;
    });
    (port, runtime)
}

/// The gateway ends the conversation stream: on a PTY the screen is up, the
/// event loop sees the stream close and the console exits with code 3
/// saying so (Embra #21: it exited 0 for this, and the cause was nowhere).
#[test]
fn a_conversation_the_gateway_ends_is_exit_3_with_the_cause() {
    if !std::path::Path::new("/dev/ptmx").exists() {
        eprintln!("skipped: this host has no /dev/ptmx");
        return;
    }
    let (port, _runtime) = gateway_with(true);
    let ended = run_console_on_a_pty(&format!("http://127.0.0.1:{port}"));
    assert_eq!(ended.code, Some(3), "it said:\n{}", ended.said.escape_debug());
    assert!(
        ended.said.contains("[embra-console] the gateway ended the conversation stream"),
        "{}",
        ended.said.escape_debug()
    );
    // stderr is the terminal here, as on the web PTY: no log is installed,
    // and the pane says the cause once, not as a log line too.
    assert_eq!(ended.said.matches("the gateway ended the conversation stream").count(), 1, "{}", ended.said.escape_debug());
    assert!(!ended.said.contains("ERROR embra_console"), "{}", ended.said.escape_debug());
}

/// When stderr is not a terminal — the serial line, where embrad sends it
/// to `/embra/ephemeral/embra-console.log` — the console's lines and its
/// exit cause are written there through `tracing`, each with its time and
/// level. The file was 0 bytes on every boot before (Embra, 2026-11-01):
/// the lines went to stdout alone, and the `tracing` calls had no
/// subscriber.
#[test]
fn the_log_carries_the_consoles_lines_and_its_exit_cause_when_stderr_is_a_file() {
    let ended = run_console(&format!("http://127.0.0.1:{}", closed_port()));
    assert_eq!(ended.code, Some(2), "it said:\n{}", ended.said);
    let log = &ended.logged;
    assert!(log.contains(" INFO embra_console: starting"), "{log}");
    assert!(log.contains(" INFO embra_console: connecting to http://127.0.0.1:"), "{log}");
    assert!(log.contains("ERROR embra_console: FATAL: "), "{log}");
    assert!(log.contains(" code=2"), "{log}");
    // Every line starts with its time, RFC 3339 in UTC.
    for line in log.lines() {
        let b = line.as_bytes();
        assert!(
            b.len() > 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':' && line.contains("Z "),
            "{line}"
        );
    }
}

/// The console reaches its gateway, opens its conversation, and then
/// cannot set up its screen: here it has no terminal at all. In the image
/// it was a terminal that did not answer.
#[test]
fn a_screen_that_cannot_be_set_up_is_an_exit_not_a_sleep() {
    let (port, _runtime) = gateway();
    let ended = run_console(&format!("http://127.0.0.1:{port}"));
    assert_eq!(ended.code, Some(1), "it said:\n{}", ended.said);
    // It got as far as its screen...
    assert!(ended.said.contains("[TUI] conversation opened"), "{}", ended.said);
    // ...and says what stopped it there.
    assert!(ended.said.contains("[embra-console] TUI error: "), "{}", ended.said);
}
