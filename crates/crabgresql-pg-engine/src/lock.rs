//! A minimal per-table lock, reproducing PostgreSQL's `AccessShareLock` vs
//! `AccessExclusiveLock` conflict rule for TRUNCATE.
//!
//! TRUNCATE swaps the table's physical file (a new relfilenode) in on commit and
//! unlinks the old file. On this engine — which has no other table-level locking
//! — that swap must be serialized against concurrent readers and writers of the
//! same table, otherwise a reader could iterate a file that TRUNCATE is about to
//! unlink, or a writer's committed rows could vanish into a discarded file. So:
//!
//! * every scan / fetch / insert / update / delete takes a **shared** guard for
//!   the duration of the operation (a scan holds it for the whole iterator life);
//! * TRUNCATE takes an **exclusive** hold, kept until its transaction ends and
//!   released by the [`crabgresql_txn::TxnFinalize`] hook.
//!
//! Shared and exclusive conflict; shared with shared do not. A transaction that
//! holds the exclusive lock is granted its own shared requests immediately
//! (read-your-own-truncate), so it never self-blocks.
//!
//! The `TableAm` methods are infallible, so a conflicting acquisition **blocks**
//! (faithful to `AccessShare` waiting for `AccessExclusive`) rather than erroring.
//! Surfacing `55P03 lock_not_available` with a bounded timeout would require
//! widening the `TableAm` trait to return `Result`; that is a deliberate
//! follow-up (see the plan and `query.rs`'s `TODO(perf)` about moving statement
//! execution off the reactor).

use std::sync::{Arc, Condvar, Mutex};

use crabgresql_txn::Xid;

#[derive(Default)]
struct LockInner {
    /// The transaction holding the table exclusively (a pending TRUNCATE), or
    /// `None`. Held until that transaction commits or aborts.
    exclusive: Option<Xid>,
    /// Count of in-flight shared holders (readers and writers).
    shared: usize,
}

/// A per-`HeapTable` lock. Held behind an `Arc` so a scan's [`SharedGuard`] can
/// outlive a borrow of the table and keep the file pinned for the iterator.
pub struct TableLock {
    inner: Mutex<LockInner>,
    cond: Condvar,
}

impl TableLock {
    pub fn new() -> TableLock {
        TableLock {
            inner: Mutex::new(LockInner::default()),
            cond: Condvar::new(),
        }
    }

    /// Acquire a shared hold for `xid`. Grants immediately when no other
    /// transaction holds the table exclusively (an exclusive hold by `xid` itself
    /// is fine — that is the truncater reading its own new file); otherwise waits
    /// until the exclusive holder finishes. The returned guard releases the hold
    /// on drop.
    pub fn acquire_shared(self: &Arc<Self>, xid: Xid) -> SharedGuard {
        let mut inner = self.inner.lock().unwrap();
        loop {
            match inner.exclusive {
                None => break,
                Some(holder) if holder == xid => break,
                Some(_) => inner = self.cond.wait(inner).unwrap(),
            }
        }
        inner.shared += 1;
        SharedGuard {
            lock: Arc::clone(self),
        }
    }

    /// Acquire the exclusive hold for `xid`, waiting until no other transaction
    /// holds the table exclusively and no shared operations are in flight. The
    /// hold is kept until [`TableLock::release_exclusive`] (called by the commit
    /// or abort hook). Re-entrant: a second TRUNCATE by the same transaction
    /// re-acquires trivially.
    pub fn acquire_exclusive(&self, xid: Xid) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.exclusive == Some(xid) {
                return;
            }
            if inner.exclusive.is_none() && inner.shared == 0 {
                inner.exclusive = Some(xid);
                return;
            }
            inner = self.cond.wait(inner).unwrap();
        }
    }

    /// Release `xid`'s exclusive hold, if it holds one. Wakes any waiters.
    pub fn release_exclusive(&self, xid: Xid) {
        let mut inner = self.inner.lock().unwrap();
        if inner.exclusive == Some(xid) {
            inner.exclusive = None;
            self.cond.notify_all();
        }
    }
}

/// Releases a shared hold on drop. A scan moves this into its iterator so the
/// table's file cannot be unlinked out from under an in-progress scan.
pub struct SharedGuard {
    lock: Arc<TableLock>,
}

impl Drop for SharedGuard {
    fn drop(&mut self) {
        let mut inner = self.lock.inner.lock().unwrap();
        inner.shared -= 1;
        // Waking on the last shared release lets a waiting exclusive acquirer
        // (which needs shared == 0) proceed.
        if inner.shared == 0 {
            self.lock.cond.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_holds_coexist() {
        let lock = Arc::new(TableLock::new());
        let a = lock.acquire_shared(Xid(3));
        let b = lock.acquire_shared(Xid(4));
        assert_eq!(lock.inner.lock().unwrap().shared, 2);
        drop(a);
        drop(b);
        assert_eq!(lock.inner.lock().unwrap().shared, 0);
    }

    #[test]
    fn exclusive_excludes_and_releases() {
        let lock = Arc::new(TableLock::new());
        lock.acquire_exclusive(Xid(3));
        assert_eq!(lock.inner.lock().unwrap().exclusive, Some(Xid(3)));
        // The holder may still take shared holds (read-your-own-truncate).
        let g = lock.acquire_shared(Xid(3));
        drop(g);
        lock.release_exclusive(Xid(3));
        assert_eq!(lock.inner.lock().unwrap().exclusive, None);
    }

    #[test]
    fn exclusive_is_reentrant_for_same_xid() {
        let lock = Arc::new(TableLock::new());
        lock.acquire_exclusive(Xid(3));
        lock.acquire_exclusive(Xid(3)); // must not block
        lock.release_exclusive(Xid(3));
    }

    #[test]
    fn exclusive_waits_for_shared_to_drain() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        let lock = Arc::new(TableLock::new());
        let held = lock.acquire_shared(Xid(3));
        let got_exclusive = Arc::new(AtomicBool::new(false));

        let l2 = Arc::clone(&lock);
        let flag = Arc::clone(&got_exclusive);
        let t = thread::spawn(move || {
            l2.acquire_exclusive(Xid(4));
            flag.store(true, Ordering::SeqCst);
            l2.release_exclusive(Xid(4));
        });

        thread::sleep(Duration::from_millis(50));
        assert!(!got_exclusive.load(Ordering::SeqCst), "exclusive must wait for the shared hold");
        drop(held);
        t.join().unwrap();
        assert!(got_exclusive.load(Ordering::SeqCst));
    }
}
