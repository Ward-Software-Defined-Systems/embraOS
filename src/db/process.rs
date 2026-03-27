use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use super::client::WardsonDbClient;
use super::error::WardsonDbError;

const WARDSONDB_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const WARDSONDB_POLL_INTERVAL: Duration = Duration::from_millis(500);
const WARDSONDB_PORT: u16 = 8090;

pub struct WardsonDbProcess {
    pub child: Child,
    pub client: WardsonDbClient,
}

pub async fn start_wardsondb() -> Result<WardsonDbProcess> {
    let data_dir =
        std::env::var("WARDSONDB_DATA_DIR").unwrap_or_else(|_| "/embra/data/wardsondb".into());

    info!("Starting WardSONDB on port {} with data dir {}", WARDSONDB_PORT, data_dir);

    // Ensure data directory exists
    tokio::fs::create_dir_all(&data_dir).await?;

    // Raise file descriptor limit for WardSONDB (fjall needs >= 4096, recommends 65536)
    #[cfg(unix)]
    {
        let rlim = libc::rlimit {
            rlim_cur: 65536,
            rlim_max: 65536,
        };
        let result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
        if result == 0 {
            info!("Set file descriptor limit to 65536");
        } else {
            warn!("Failed to set file descriptor limit (errno: {}), WardSONDB may hit 'too many open files'",
                  std::io::Error::last_os_error());
        }
    }

    let child = Command::new("wardsondb")
        .args([
            "--port",
            &WARDSONDB_PORT.to_string(),
            "--data-dir",
            &data_dir,
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let client = WardsonDbClient::new(WARDSONDB_PORT);

    // Poll health endpoint until ready
    let result = timeout(WARDSONDB_STARTUP_TIMEOUT, async {
        loop {
            if client.health().await.unwrap_or(false) {
                return;
            }
            sleep(WARDSONDB_POLL_INTERVAL).await;
        }
    })
    .await;

    if result.is_err() {
        error!("WardSONDB failed to start within {}s", WARDSONDB_STARTUP_TIMEOUT.as_secs());
        return Err(WardsonDbError::StartupTimeout(WARDSONDB_STARTUP_TIMEOUT.as_secs()).into());
    }

    info!("WardSONDB is healthy");
    Ok(WardsonDbProcess { child, client })
}
