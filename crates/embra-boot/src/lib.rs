//! The boot contract embra-init, embrad and embra-update share: the slot the
//! kernel line names, where a partition is by its GPT name, GRUB's
//! environment block on the EFI System Partition, the A/B state it carries,
//! the release stamp a slot ships with, the boot console a message for the
//! operator goes to (the serial line and the display both) and a prompt reads
//! from, the network cards of the machine, the lease embrad records and the
//! console URLs that follow from it, the web console's access token on STATE,
//! what embrad made of the clock at boot (and the date arithmetic that
//! reads the release stamp's build time), and what an installer needs: the
//! layout, the medium, the disks. The native models (experimental) have
//! their contract here too: the catalogue, and the records embrad, the brain
//! and the updater pass each other. So has the licence the OS is released
//! under: where its text is in the root filesystem, and its version line. No runtime
//! and no dependency but `libc`: embra-init runs it from the initramfs,
//! before anything else exists.

pub mod clock;
pub mod cmdline;
pub mod console;
pub mod disks;
pub mod esp;
pub mod grubenv;
pub mod layout;
pub mod license;
pub mod loopdev;
pub mod media;
pub mod names;
pub mod native_model;
pub mod net;
pub mod net_lease;
pub mod net_settings;
pub mod partitions;
pub mod release;
pub mod slot;
pub mod vt;
pub mod web_token;
pub mod wifi;
pub mod state;
pub mod status;

pub use clock::ClockStatus;
pub use cmdline::{BootMode, Cmdline, CmdlineError, KioskMode, TimeServer};
pub use grubenv::{GrubEnv, GrubEnvError};
pub use release::{Release, ReleaseError};
pub use slot::Slot;
pub use state::{BootState, SlotStatus};
pub use status::BootStatus;
