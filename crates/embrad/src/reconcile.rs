//! Reconciliation loop for embrad.
//!
//! Continuously monitors service health and restarts failed services,
//! answers the operator's request for a new web console token
//! (`Supervisor::rotate_web_token`),
//! reaps the orphans that reparent to PID 1 (#18), and handles the signals
//! that stop the machine — SIGTERM and SIGINT for a restart, SIGUSR2 for a
//! power-off (`Stop`) — during a restart's backoff and health wait as well,
//! not after them (#63).

use std::future::Future;
use std::pin::pin;

use crate::supervisor::{ServiceStatus, Stop, Supervisor};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{error, info, warn};

/// Runs until a signal asks for the machine to be stopped, and says how:
/// SIGTERM (the operator's `/reboot now`) and SIGINT restart it, SIGUSR2
/// (`/reboot halt`) powers it off. The numbers are BusyBox init's: its
/// `reboot` sends SIGTERM and its `poweroff` SIGUSR2.
pub async fn run(supervisor: &mut Supervisor) -> Stop {
    // A handler that cannot be installed is logged once and that signal is
    // never awaited; PID 1 does not panic for it.
    let mut sigterm = install(SignalKind::terminate(), "SIGTERM");
    let mut sigint = install(SignalKind::interrupt(), "SIGINT");
    let mut sigusr2 = install(SignalKind::user_defined2(), "SIGUSR2");
    let sigchld = install(SignalKind::child(), "SIGCHLD");
    let shutdown = async move {
        tokio::select! {
            _ = recv_or_pending(&mut sigterm) => ("SIGTERM", Stop::Restart),
            _ = recv_or_pending(&mut sigint) => ("SIGINT", Stop::Restart),
            _ = recv_or_pending(&mut sigusr2) => ("SIGUSR2", Stop::PowerOff),
        }
    };
    run_until(supervisor, shutdown, sigchld).await
}

/// The loop, with the shutdown signal as a future (a test passes a channel)
/// and the SIGCHLD handler as given. Every tick checks the services and
/// restarts the failed ones; a restart's backoff and health wait run under
/// the shutdown future as well, so a signal is answered during them (#63).
/// A SIGCHLD, and every tick, reaps the orphans that reparented to PID 1.
/// What comes back is how the signal asked for the machine to be stopped.
pub(crate) async fn run_until(
    supervisor: &mut Supervisor,
    shutdown: impl Future<Output = (&'static str, Stop)>,
    mut sigchld: Option<Signal>,
) -> Stop {
    let mut shutdown = pin!(shutdown);
    let health_interval = tokio::time::Duration::from_secs(5);
    let mut health_tick = tokio::time::interval(health_interval);

    info!("Reconciliation loop started (health check every {:?})", health_interval);

    loop {
        tokio::select! {
            (name, stop) = &mut shutdown => {
                info!("Received {name} — initiating shutdown ({stop})");
                break stop;
            }
            _ = recv_or_pending(&mut sigchld) => {
                reap(supervisor.child_pids());
            }
            _ = health_tick.tick() => {
                tokio::select! {
                    (name, stop) = &mut shutdown => {
                        info!("Received {name} during a restart — initiating shutdown ({stop})");
                        break stop;
                    }
                    _ = tick(supervisor) => {}
                }
                reap(supervisor.child_pids());
            }
        }
    }
}

/// One tick: the operator's request for a new web console token, when one
/// is on STATE, and then the pass over the services. Cancelled by a shutdown
/// signal, which stops everything next.
async fn tick(supervisor: &mut Supervisor) {
    if supervisor.web_token_rotation_asked() {
        supervisor.rotate_web_token().await;
    }
    reconcile_once(supervisor).await;
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

/// One sweep of the reaper with the services' pids and the helpers' (the
/// DHCP clients, `net.rs`) left alone, under the helpers' lock: a helper is
/// started with that lock held, so its exit cannot be taken between its
/// start and its registration.
fn reap(mut owned: Vec<u32>) -> usize {
    crate::net::with_helpers(|helpers| {
        owned.extend_from_slice(helpers);
        reap_orphans(&owned)
    })
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

    /// A DHCP client is no service of the supervisor's, and its exit is its
    /// owner's to collect all the same: its pid is on the helpers' list
    /// (`net.rs`), which every sweep leaves alone.
    #[test]
    fn the_reaper_leaves_a_registered_helper_to_its_owner() {
        let _children = crate::supervisor::test_support::CHILDREN.blocking_lock();
        let mut helper = std::process::Command::new("true").spawn().unwrap();
        let pid = helper.id();
        crate::net::helpers().push(pid);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(reap(Vec::new()), 0, "the helper's exit is not taken");
        assert!(helper.wait().unwrap().success(), "and is still there for its owner");
        crate::net::helpers().retain(|p| *p != pid);
        // Off the list, an exited child is anybody's: the sweep takes it.
        let stray = std::process::Command::new("true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(reap(Vec::new()), 1, "a child nobody owns is reaped");
        drop(stray);
    }
}
