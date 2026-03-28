//! embra-init — Initramfs early boot binary for embraOS.
//!
//! Responsibilities:
//! 1. Mount SquashFS rootfs
//! 2. Mount STATE partition (ext4)
//! 3. Mount DATA partition (ext4)
//! 4. Mount EPHEMERAL (tmpfs)
//! 5. pivot_root and exec embrad as PID 1

fn main() {
    eprintln!("[embra-init] embraOS early boot");
    eprintln!("[embra-init] TODO: implement in Doc 06");
    // Placeholder — will be implemented in Doc 06 (embra-init & Buildroot Image)
    std::process::exit(1);
}
