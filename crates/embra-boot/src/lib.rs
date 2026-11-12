//! The boot contract embra-init, embrad and embra-update share: the slot the
//! kernel line names, where a partition is by its GPT name, GRUB's
//! environment block on the EFI System Partition, the A/B state it carries,
//! the release stamp a slot ships with, the boot console a message for the
//! operator goes to (the serial line and the display both) and a prompt reads
//! from, the network lease embrad records and the console URLs that follow
//! from it, what embrad made of the clock at boot (and the date arithmetic that
//! reads the release stamp's build time), and what an installer needs: the
//! layout, the medium, the disks. No runtime
//! and no dependency but `libc`: embra-init runs it from the initramfs,
//! before anything else exists.

pub mod clock;
pub mod cmdline;
pub mod console;
pub mod disks;
pub mod esp;
pub mod grubenv;
pub mod layout;
pub mod loopdev;
pub mod media;
pub mod net_lease;
pub mod partitions;
pub mod release;
pub mod slot;
pub mod vt;
pub mod state;
pub mod status;

pub use clock::ClockStatus;
pub use cmdline::{BootMode, Cmdline, CmdlineError, KioskMode, TimeServer};
pub use grubenv::{GrubEnv, GrubEnvError};
pub use release::{Release, ReleaseError};
pub use slot::Slot;
pub use state::{BootState, SlotStatus};
pub use status::BootStatus;
