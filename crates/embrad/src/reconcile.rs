//! Reconciliation loop for embrad.
//!
//! Continuously monitors service health and restarts failed services.
//! Also handles SIGTERM/SIGINT for graceful shutdown.

use crate::supervisor::{Supervisor, ServiceStatus};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn, error};

pub async fn run(supervisor: &mut Supervisor) {
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");

    let health_interval = tokio::time::Duration::from_secs(5);
    let mut health_tick = tokio::time::interval(health_interval);

    info!("Reconciliation loop started (health check every {:?})", health_interval);

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM — initiating shutdown");
                break;
            }
            _ = sigint.recv() => {
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
