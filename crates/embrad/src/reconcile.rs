//! Reconciliation loop for embrad.
//!
//! Continuously monitors service health and restarts failed services.
//! Also handles SIGTERM/SIGINT for graceful shutdown.

use crate::supervisor::{ServiceStatus, Supervisor};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{error, info, warn};

pub async fn run(supervisor: &mut Supervisor) {
    // A handler that cannot be installed is logged once and that signal is
    // never awaited; PID 1 does not panic for it.
    let mut sigterm = install(SignalKind::terminate(), "SIGTERM");
    let mut sigint = install(SignalKind::interrupt(), "SIGINT");

    let health_interval = tokio::time::Duration::from_secs(5);
    let mut health_tick = tokio::time::interval(health_interval);

    info!("Reconciliation loop started (health check every {:?})", health_interval);

    loop {
        tokio::select! {
            _ = recv_or_pending(&mut sigterm) => {
                info!("Received SIGTERM — initiating shutdown");
                break;
            }
            _ = recv_or_pending(&mut sigint) => {
                info!("Received SIGINT — initiating shutdown");
                break;
            }
            _ = health_tick.tick() => {
                // Check all services
                for i in 0..supervisor.service_count() {
                    if *supervisor.service_status(i) == ServiceStatus::Halted {
                        continue; // Don't check halted services
                    }

                    let alive = supervisor.check_service(i).await;
                    if !alive && matches!(supervisor.service_status(i), ServiceStatus::Failed(_)) {
                        warn!("Service {} failed — attempting restart", supervisor.service_name(i));
                        if let Err(e) = supervisor.restart_service(i).await {
                            error!("Failed to restart {}: {}", supervisor.service_name(i), e);
                        }
                    }
                }
            }
        }
    }
}

/// The handler for one signal, or `None` with the failure logged.
fn install(kind: SignalKind, name: &str) -> Option<Signal> {
    match signal(kind) {
        Ok(s) => Some(s),
        Err(e) => {
            error!("Failed to install the {name} handler: {e}; {name} will not be handled");
            None
        }
    }
}

/// The next delivery of a signal, or a future that never completes when
/// its handler could not be installed.
async fn recv_or_pending(sig: &mut Option<Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}
