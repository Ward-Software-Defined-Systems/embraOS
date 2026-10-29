//! Cooperative cancellation of a read that outlived its timeout.
//!
//! A read runs on the blocking pool, and a timed-out `spawn_blocking` task
//! cannot be killed: without this it scanned to the end for a client that
//! had already been answered, holding a pool thread and the engine's
//! snapshot. `with_query_timeout` sets a token for the thread it runs the
//! read on and cancels it when the timeout fires; every scan loop asks
//! `checkpoint` every `CHECK_EVERY` entries and stops with the timeout
//! error. A write passes no timeout and gets no token, so a mutation is
//! never cut after its client gave up.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::AppError;

/// Entries a scan loop visits between two looks at the token.
pub(crate) const CHECK_EVERY: u64 = 1000;

/// The handle the timeout side holds; a clone of it is the read's.
#[derive(Clone, Debug)]
pub(crate) struct CancelToken {
    cancelled: Arc<AtomicBool>,
    timeout_secs: u64,
}

impl CancelToken {
    pub(crate) fn new(timeout_secs: u64) -> Self {
        CancelToken {
            cancelled: Arc::new(AtomicBool::new(false)),
            timeout_secs,
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<CancelToken>> = const { RefCell::new(None) };
}

/// Run `f` with `token` as this thread's read token. The token is cleared
/// when `f` returns or unwinds: the blocking pool reuses its threads, and
/// a token left behind would cut another request's read.
pub(crate) fn scoped<R>(token: &CancelToken, f: impl FnOnce() -> R) -> R {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            CURRENT.with(|current| *current.borrow_mut() = None);
        }
    }
    CURRENT.with(|current| *current.borrow_mut() = Some(token.clone()));
    let _clear = Clear;
    f()
}

/// A scan loop's checkpoint. `seen` is the loop's own count of entries
/// visited; every `CHECK_EVERY` of them the thread's token is read, and a
/// cancelled one is the timeout the request was answered with. Outside a
/// `scoped` read nothing is ever cancelled.
pub(crate) fn checkpoint(seen: &mut u64) -> Result<(), AppError> {
    *seen += 1;
    if !(*seen).is_multiple_of(CHECK_EVERY) {
        return Ok(());
    }
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(token) if token.is_cancelled() => Err(AppError::QueryTimeout(token.timeout_secs)),
        _ => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::{MemoryConfig, Storage};
    use serde_json::json;

    fn storage_with(docs: usize) -> (tempfile::TempDir, Storage) {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage =
            Storage::open_with_config(tmp.path(), "fjall", MemoryConfig::default()).unwrap();
        storage.create_collection("c").unwrap();
        let docs: Vec<_> = (0..docs).map(|i| json!({"n": i})).collect();
        storage.bulk_insert_documents("c", docs).unwrap();
        (tmp, storage)
    }

    /// A scan under a cancelled token stops at its next checkpoint with the
    /// timeout it was given; the same scan outside any scope, or under a
    /// live token, runs to the end.
    #[test]
    fn a_cancelled_token_stops_a_scan_at_its_next_checkpoint() {
        let (_tmp, storage) = storage_with(2 * CHECK_EVERY as usize + 500);
        let token = CancelToken::new(7);
        token.cancel();
        let err = scoped(&token, || storage.scan_all_documents("c")).unwrap_err();
        assert!(matches!(err, AppError::QueryTimeout(7)), "{err:?}");

        let live = CancelToken::new(7);
        let docs = scoped(&live, || storage.scan_all_documents("c")).unwrap();
        assert_eq!(docs.len(), 2 * CHECK_EVERY as usize + 500);

        let docs = storage.scan_all_documents("c").unwrap();
        assert_eq!(docs.len(), 2 * CHECK_EVERY as usize + 500);
    }

    /// The token is this thread's for the scope alone.
    #[test]
    fn the_token_is_cleared_when_the_scope_ends() {
        let token = CancelToken::new(3);
        token.cancel();
        let mut seen = CHECK_EVERY - 1;
        scoped(&token, || {
            assert!(checkpoint(&mut seen).is_err());
        });
        let mut seen = CHECK_EVERY - 1;
        assert!(checkpoint(&mut seen).is_ok(), "no token outside the scope");
        let result = std::panic::catch_unwind(|| {
            scoped(&token, || panic!("a read that unwinds"));
        });
        assert!(result.is_err());
        let mut seen = CHECK_EVERY - 1;
        assert!(checkpoint(&mut seen).is_ok(), "an unwound scope left no token");
    }
}
