//! Reconciliation loop for embrad.
//!
//! Continuously monitors service health and restarts failed services,
//! reaps the orphans that reparent to PID 1 (#18), and handles
//! SIGTERM/SIGINT for graceful shutdown — during a restart's backoff and
//! health wait as well, not after them (#63).

use std::future::Future;
use std::pin::pin;

use crate::supervisor::{ServiceStatus, Supervisor};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{error, info, warn};

pub async fn run(supervisor: &mut Supervisor) {
    // A handler that cannot be installed is logged once and that signal is
    // never awaited; PID 1 does not panic for it.
    let mut sigterm = install(SignalKind::terminate(), "SIGTERM");
    let mut sigint = install(SignalKind::interrupt(), "SIGINT");
    let sigchld = install(SignalKind::child(), "SIGCHLD");
    let shutdown = async move {
        tokio::select! {
            _ = recv_or_pending(&mut sigterm) => "SIGTERM",
            _ = recv_or_pending(&mut sigint) => "SIGINT",
        }
    };
    run_until(supervisor, shutdown, sigchld).await;
}

/// The loop, with the shutdown signal as a future (a test passes a channel)
/// and the SIGCHLD handler as given. Every tick checks the services and
/// restarts the failed ones; a restart's backoff and health wait run under
/// the shutdown future as well, so a signal is answered during them (#63).
/// A SIGCHLD, and every tick, reaps the orphans that reparented to PID 1.
pub(crate) async fn run_until(
    supervisor: &mut Supervisor,
    shutdown: impl Future<Output = &'static str>,
    mut sigchld: Option<Signal>,
) {
    let mut shutdown = pin!(shutdown);
    let health_interval = tokio::time::Duration::from_secs(5);
    let mut health_tick = tokio::time::interval(health_interval);

    info!("Reconciliation loop started (health check every {:?})", health_interval);

    loop {
        tokio::select! {
            name = &mut shutdown => {
                info!("Received {name} — initiating shutdown");
                break;
            }
            _ = recv_or_pending(&mut sigchld) => {
                reap_orphans(&supervisor.child_pids());
            }
            _ = health_tick.tick() => {
                tokio::select! {
                    name = &mut shutdown => {
                        info!("Received {name} during a restart — initiating shutdown");
                        break;
                    }
                    _ = reconcile_once(supervisor) => {}
                }
                reap_orphans(&supervisor.child_pids());
            }
        }
    }
}

/// One pass over the services: a service that exited is restarted under
/// its budget. Cancelled by a shutdown signal, which stops everything next.
async fn reconcile_once(supervisor: &mut Supervisor) {
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

/// Reaps the children of this process that are not supervised services:
/// the orphans that reparented to PID 1 (a grandchild a git or ssh of the
/// brain left behind), which nobody else waits on and which would stay
/// zombies for the life of the boot (#18). Each exited child is looked at
/// with `WNOWAIT` first, so a supervised service's exit is left for tokio's
/// `Child` to collect; a sweep that finds one stops there, and the next
/// tick runs again once it has been collected. Returns the count reaped.
pub(crate) fn reap_orphans(supervised: &[u32]) -> usize {
    use nix::sys::wait::{waitid, waitpid, Id, WaitPidFlag, WaitStatus};
    let mut reaped = 0;
    loop {
        let pid = match waitid(Id::All, WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(status) => match status.pid() {
                Some(pid) => pid,
                None => break,
            },
        };
        if supervised.contains(&(pid.as_raw() as u32)) {
            break;
        }
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => reaped += 1,
        }
    }
    reaped
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An orphan that reparents here (the test makes itself a subreaper, as
    /// PID 1 is one by nature) is reaped; afterwards there is nothing to
    /// reap. A pid on the supervised list is never waited on.
    #[test]
    fn an_orphaned_grandchild_is_reaped_and_a_supervised_pid_is_left() {
        // One child-spawning test at a time: the sweeps below take every
        // exited child of this process (`supervisor::test_support`).
        let _children = crate::supervisor::test_support::CHILDREN.blocking_lock();
        nix::sys::prctl::set_child_subreaper(true).unwrap();
        // The child forks a grandchild and exits at once; the grandchild
        // outlives it, reparents to this process and exits a little later.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.2 & exit 0"])
            .spawn()
            .unwrap();
        child.wait().unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(reap_orphans(&[]) >= 1, "the orphan was reaped");
        assert_eq!(reap_orphans(&[]), 0, "nothing is left");
        // A supervised pid is left for tokio: a child that has exited and
        // is on the list stops the sweep at it.
        let mut kept = std::process::Command::new("true").spawn().unwrap();
        let pid = kept.id();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(reap_orphans(&[pid]), 0, "the supervised exit is not taken");
        assert!(kept.wait().unwrap().success(), "and is still there to collect");
    }
}
