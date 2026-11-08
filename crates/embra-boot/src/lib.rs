//! The boot contract embra-init, embrad and embra-update share: the slot the
//! kernel line names, where a partition is by its GPT name, GRUB's
//! environment block on the EFI System Partition, the A/B state it carries,
//! the release stamp a slot ships with, and the boot console a message for
//! the operator goes to (the serial line and the display both). No runtime
//! and no dependency but `libc`: embra-init runs it from the initramfs,
//! before anything else exists.

pub mod cmdline;
pub mod console;
pub mod esp;
pub mod grubenv;
pub mod partitions;
pub mod release;
pub mod slot;
pub mod state;
pub mod status;

pub use cmdline::{BootMode, Cmdline, CmdlineError};
pub use grubenv::{GrubEnv, GrubEnvError};
pub use release::{Release, ReleaseError};
pub use slot::Slot;
pub use state::{BootState, SlotStatus};
pub use status::BootStatus;
