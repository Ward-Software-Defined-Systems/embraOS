//! Maintenance boots from the GRUB menu (`embra.mode=update|backup|restore`):
//! one job on the console instead of the services, then a reboot back to
//! the menu. The jobs land with the backup and update waves; until then a
//! mode is named on the console and the machine returns to the menu.

use anyhow::Result;
use embra_boot::BootMode;
use tracing::error;

/// How long the console shows the message before the reboot.
const PAUSE: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn run(mode: BootMode) -> Result<()> {
    error!(
        "boot mode '{}' is not implemented in this build; rebooting to the menu in {} s",
        mode.name(),
        PAUSE.as_secs()
    );
    tokio::time::sleep(PAUSE).await;
    Ok(())
}
