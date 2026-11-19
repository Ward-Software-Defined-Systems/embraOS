//! Writes that a power cut must not take.
//!
//! The database answers a write once it has handed it to the kernel: fjall
//! commits with `PersistMode::Buffer`, and RocksDB writes its log without a
//! sync. The kernel writes it out when it gets to it, within about half a
//! minute. A stop of embraOS unmounts DATA and loses nothing, and the end of
//! a process loses nothing either; a power cut loses that half minute.
//! Measured on a scratch VM with the RocksDB engine: a turn's record was
//! gone after a cut 1 and 25 seconds into the turn and there after 38 and
//! 45; a turn that had ended ten seconds before the cut was gone too.
//!
//! For a turn that is not good enough: the operator's message would be lost
//! with it, and the turn's record exists so that it is not. The brain cannot
//! ask the database to sync (its REST surface has no such call, and its
//! source is not this crate's to change), but the two run on one machine and
//! the database's files are on DATA: `syncfs(2)` on that filesystem writes
//! out everything the database has handed over, the write just answered
//! included. `sessions::SessionStore` asks for it after each write of a
//! turn: the record at its start, the record as the turn goes on, and the
//! turn at its save.
//!
//! What the database writes outside a turn (a reminder that fired, a job of
//! the cron) reaches the disk with the kernel's own half minute, or with the
//! next turn's sync, whichever is first.

use std::path::Path;

/// The filesystem the database keeps its files on.
pub(crate) const DATA_MOUNT: &str = "/embra/data";

/// A sync that takes this long is said in the log: the operator waits for it
/// at the start and at the end of a turn.
const SLOW: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(test)]
thread_local! {
    static ASKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times this thread asked for DATA to be written out. A test on
/// the current-thread runtime reads its own count.
#[cfg(test)]
pub(crate) fn asked() -> u64 {
    ASKED.with(std::cell::Cell::get)
}

/// Have the kernel write DATA out, and wait until it has.
///
/// Best effort. A machine without DATA (a development host, a test) has
/// nothing to write out and says nothing. A sync that fails is said in the
/// log and the caller goes on: the write itself was answered, and reaches
/// the disk as it did before.
pub(crate) async fn sync_data() {
    #[cfg(test)]
    ASKED.with(|asked| asked.set(asked.get() + 1));
    let started = std::time::Instant::now();
    match tokio::task::spawn_blocking(|| sync_filesystem(Path::new(DATA_MOUNT))).await {
        Ok(Ok(())) => {
            let took = started.elapsed();
            if took >= SLOW {
                tracing::warn!(target: "durable", ms = took.as_millis() as u64, "DATA took long to write out");
            } else {
                tracing::debug!(target: "durable", ms = took.as_millis() as u64, "DATA written out");
            }
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(Err(e)) => tracing::warn!(target: "durable", "DATA could not be written out: {e}"),
        Err(e) => tracing::warn!(target: "durable", "the sync of DATA did not run: {e}"),
    }
}

/// `syncfs(2)` on the filesystem that holds `dir`: every write the kernel
/// holds for it goes to the disk, whichever process made it.
pub(crate) fn sync_filesystem(dir: &Path) -> std::io::Result<()> {
    let handle = std::fs::File::open(dir)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `syncfs` takes an open descriptor and reads or writes no
        // memory of this process; `handle` is open until the call returns.
        if unsafe { libc::syncfs(handle.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        handle.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filesystem_that_is_there_is_written_out_and_one_that_is_not_says_so() {
        let dir = std::env::temp_dir().join(format!("embra-durable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(dir.join("a-write"), b"handed to the kernel").expect("a write");
        sync_filesystem(&dir).expect("the filesystem of the scratch directory is synced");

        let absent = sync_filesystem(&dir.join("no-such-mount")).expect_err("nothing is there");
        assert_eq!(absent.kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host without DATA: the request is counted, nothing is said and
    /// nothing fails.
    #[tokio::test]
    async fn a_machine_without_data_has_nothing_to_write_out() {
        assert!(!Path::new(DATA_MOUNT).exists(), "this test runs on a host that is not embraOS");
        let before = asked();
        sync_data().await;
        sync_data().await;
        assert_eq!(asked() - before, 2);
    }
}
