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
    said: String,
}

/// Run the console against `apid_addr` until it exits. Its standard
/// streams are pipes and it has no controlling terminal: crossterm falls
/// back to `/dev/tty` when stdin is not a terminal, and the terminal
/// `cargo test` was started from must not become the console's.
fn run_console(apid_addr: &str) -> Ended {
    let mut cmd = Command::new(CONSOLE);
    cmd.args(["--apid-addr", apid_addr])
        .env("EMBRA_WEB_PTY", "1")
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
            let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
            said.push_str(&String::from_utf8_lossy(&out.stderr));
            return Ended { code: status.code(), said };
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
/// conversation opens and stays open, and says nothing.
struct Gateway;

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
    async fn stop_turn(&self, _: Request<StopTurnRequest>) -> Result<Response<StopTurnResponse>, Status> {
        Err(not_here())
    }
    async fn put_media(&self, _: Request<PutMediaRequest>) -> Result<Response<PutMediaResponse>, Status> {
        Err(not_here())
    }
    async fn get_media(&self, _: Request<GetMediaRequest>) -> Result<Response<GetMediaResponse>, Status> {
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
            .add_service(EmbraApiServer::new(Gateway))
            .serve_with_incoming(incoming)
            .await;
    });
    (port, runtime)
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
