//! embra-apid — gRPC + REST API gateway for embraOS.

mod config;
mod grpc;
mod rest;
mod proxy;

use config::ApidConfig;
use grpc::EmbraApiImpl;
use proxy::BackendConnections;

use embra_common::proto::apid::embra_api_server::EmbraApiServer;
use tonic::transport::Server as TonicServer;
use tracing::info;

/// The default log filter: this crate and every other at INFO. `RUST_LOG`
/// overrides it (`log_filter`). A bare `tracing_subscriber::fmt::init()`
/// read `RUST_LOG` too, and when it was unset — embrad sets it for no
/// child — fell back to ERROR alone, so the log embrad captured for this
/// service stayed empty on the image.
const DEFAULT_LOG_DIRECTIVE: &str = "info";

/// The filter `main` installs: `RUST_LOG` when it is set and parses,
/// otherwise the default.
fn log_filter() -> tracing_subscriber::EnvFilter {
    log_filter_from(std::env::var("RUST_LOG").ok().as_deref())
}

fn log_filter_from(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    rust_log
        .and_then(|spec| tracing_subscriber::EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_DIRECTIVE))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(log_filter()).with_ansi(false).init();
    let config = ApidConfig::from_args();

    info!("embra-apid starting (gRPC={}, REST={})", config.grpc_port, config.rest_port);

    let backends = BackendConnections::new(
        config.brain_addr.clone(),
        config.trust_addr.clone(),
    );

    // gRPC server
    let grpc_addr = format!("0.0.0.0:{}", config.grpc_port).parse()?;
    let grpc_service = EmbraApiImpl::new(backends.clone());

    let grpc_handle = tokio::spawn(async move {
        info!("gRPC server listening on {}", grpc_addr);
        TonicServer::builder()
            .add_service(
                // PutMedia requests from embra-web carry raw image bytes.
                EmbraApiServer::new(grpc_service)
                    .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES),
            )
            .serve(grpc_addr)
            .await
            .expect("gRPC server failed");
    });

    // REST server
    let rest_addr: std::net::SocketAddr = format!("0.0.0.0:{}", config.rest_port).parse()?;
    let rest_router = rest::build_router(backends.clone());

    let rest_handle = tokio::spawn(async move {
        info!("REST server listening on {}", rest_addr);
        let listener = tokio::net::TcpListener::bind(rest_addr).await.expect("REST bind failed");
        axum::serve(listener, rest_router).await.expect("REST server failed");
    });

    // Wait for either server to exit (shouldn't happen)
    tokio::select! {
        _ = grpc_handle => { info!("gRPC server exited"); }
        _ = rest_handle => { info!("REST server exited"); }
    }

    Ok(())
}

#[cfg(test)]
mod log_filter_tests {
    use super::log_filter_from;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A writer the subscriber under test appends to.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The lines a subscriber built like `main`'s writes under a filter,
    /// with the sink in place of stderr.
    fn logged_under(rust_log: Option<&str>, emit: impl FnOnce()) -> String {
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(log_filter_from(rust_log))
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    /// Without `RUST_LOG`, the crate's INFO lines are written and nothing
    /// quieter; another crate's warnings still reach the log. The bare
    /// `fmt::init()` wrote ERROR alone here, and the service's log on the
    /// image was empty.
    #[test]
    fn the_default_filter_passes_the_crates_info_lines_and_nothing_quieter() {
        let out = logged_under(None, || {
            tracing::info!(target: "embra_apid", "an info line of the service");
            tracing::debug!(target: "embra_apid", "a debug line of the service");
            tracing::warn!(target: "h2::proto", "another crate's warning");
            tracing::debug!(target: "h2::proto", "another crate's debug line");
        });
        assert!(out.contains("an info line of the service"), "{out}");
        assert!(!out.contains("a debug line of the service"), "{out}");
        assert!(out.contains("another crate's warning"), "{out}");
        assert!(!out.contains("another crate's debug line"), "{out}");
    }

    /// `RUST_LOG` wins when it parses; one that does not falls back to the
    /// default instead of silencing the service.
    #[test]
    fn rust_log_overrides_the_default_and_an_unparseable_one_falls_back() {
        let out = logged_under(Some("embra_apid=debug"), || {
            tracing::debug!(target: "embra_apid", "a debug line of the service");
        });
        assert!(out.contains("a debug line of the service"), "{out}");
        let out = logged_under(Some("=not a directive="), || {
            tracing::info!(target: "embra_apid", "still logged");
        });
        assert!(out.contains("still logged"), "{out}");
    }
}
