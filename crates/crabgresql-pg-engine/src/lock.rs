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
//! Shared and exclusive conflict; shared with shared do not. Holds are keyed by a
//! **[`LockOwner`]** (a session, not a transaction), so — exactly like
//! PostgreSQL's lock manager, where a backend never self-conflicts — the *same*
//! owner can upgrade its own shared holds to exclusive: `TRUNCATE`-ing a table the
//! same session has an open cursor on does not self-deadlock, while another
//! session's cursor still blocks the TRUNCATE. A read-only-so-far transaction
//! scans with [`Xid::INVALID`], so the owner (not the XID) is the stable key that
//! makes this work.
//!
//! The `TableAm` methods are infallible, so a conflicting acquisition **blocks**
//! (faithful to `AccessShare` waiting for `AccessExclusive`) rather than erroring.
//! Surfacing `55P03 lock_not_available` with a bounded timeout would require
//! widening the `TableAm` trait to return `Result`; that is a deliberate
//! follow-up (see the plan and `query.rs`'s `TODO(perf)` about moving statement
//! execution off the reactor).
//!
//! Performance note (review finding #9): the shared acquire takes this per-table
//! `Mutex` on every DML operation, but it is required for cross-session
//! correctness, is uncontended in the common (no-TRUNCATE) case, and is taken
//! once per scan (not per row) — one more short critical section among the
//! per-page `bufpool` locks each operation already takes. A lock-free fast path
//! is incompatible with the per-owner upgrade bookkeeping this fix needs, so the
//! mutex stays.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use crabgresql_txn::LockOwner;

#[derive(Default)]
struct LockInner {
    /// The owner holding the table exclusively (a pending TRUNCATE), or `None`.
    /// Held until that transaction commits or aborts.
    exclusive: Option<LockOwner>,
    /// Per-owner count of in-flight shared holders (readers and writers). Keyed
    /// by owner so an exclusive acquire can tell its own holds apart from others'.
    shared: HashMap<LockOwner, u32>,
}

impl LockInner {
    /// Whether any owner *other than* `owner` currently holds a shared hold — the
    /// only thing (besides another exclusive) that blocks `owner`'s exclusive.
    fn foreign_shared(&self, owner: LockOwner) -> bool {
        self.shared.keys().any(|k| *k != owner)
    }
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

    /// Acquire a shared hold for `owner`. Grants immediately when no *other* owner
    /// holds the table exclusively (an exclusive hold by `owner` itself is fine —
    /// that is the truncater reading its own new file); otherwise waits until the
    /// exclusive holder finishes. The returned guard releases the hold on drop.
    pub fn acquire_shared(self: &Arc<Self>, owner: LockOwner) -> SharedGuard {
        let mut inner = self.inner.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        loop {
            match inner.exclusive {
                None => break,
                Some(holder) if holder == owner => break,
                Some(_) => {
                    inner = self
                        .cond
                        .wait(inner)
                        .unwrap_or_else(|_| panic!("mutex poisoned"))
                }
            }
        }
        *inner.shared.entry(owner).or_default() += 1;
        SharedGuard {
            lock: Arc::clone(self),
            owner,
        }
    }

    /// Acquire the exclusive hold for `owner`, waiting until no *other* owner holds
    /// the table exclusively and no *other* owner holds a shared hold. `owner`'s
    /// own shared holds do not block it (lock upgrade), so a session can TRUNCATE a
    /// table it has an open cursor on. The hold is kept until
    /// [`TableLock::release_exclusive`] (called by the commit or abort hook).
    /// Re-entrant: a second TRUNCATE by the same owner re-acquires trivially.
    pub fn acquire_exclusive(&self, owner: LockOwner) {
        let mut inner = self.inner.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        loop {
            let exclusive_ok = match inner.exclusive {
                None => true,
                Some(holder) => holder == owner,
            };
            if exclusive_ok && !inner.foreign_shared(owner) {
                inner.exclusive = Some(owner);
                return;
            }
            inner = self
                .cond
                .wait(inner)
                .unwrap_or_else(|_| panic!("mutex poisoned"));
        }
    }

    /// Release `owner`'s exclusive hold, if it holds one. Wakes any waiters.
    pub fn release_exclusive(&self, owner: LockOwner) {
        let mut inner = self.inner.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        if inner.exclusive == Some(owner) {
            inner.exclusive = None;
            self.cond.notify_all();
        }
    }
}

/// Releases a shared hold on drop. A scan moves this into its iterator so the
/// table's file cannot be unlinked out from under an in-progress scan.
pub struct SharedGuard {
    lock: Arc<TableLock>,
    owner: LockOwner,
}

impl Drop for SharedGuard {
    fn drop(&mut self) {
        let mut inner = self
            .lock
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if let Some(count) = inner.shared.get_mut(&self.owner) {
            *count -= 1;
            if *count == 0 {
                inner.shared.remove(&self.owner);
            }
        }
        // A waiting exclusive acquirer may now be unblocked (its last conflicting
        // shared holder just left).
        self.lock.cond.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(n: u64) -> LockOwner {
        LockOwner(n)
    }

    #[test]
    fn shared_holds_coexist() {
        let lock = Arc::new(TableLock::new());
        let a = lock.acquire_shared(owner(3));
        let b = lock.acquire_shared(owner(4));
        assert_eq!(lock.inner.lock().unwrap().shared.values().sum::<u32>(), 2);
        drop(a);
        drop(b);
        assert!(lock.inner.lock().unwrap().shared.is_empty());
    }

    #[test]
    fn exclusive_excludes_and_releases() {
        let lock = Arc::new(TableLock::new());
        lock.acquire_exclusive(owner(3));
        assert_eq!(lock.inner.lock().unwrap().exclusive, Some(owner(3)));
        // The holder may still take shared holds (read-your-own-truncate).
        let g = lock.acquire_shared(owner(3));
        drop(g);
        lock.release_exclusive(owner(3));
        assert_eq!(lock.inner.lock().unwrap().exclusive, None);
    }

    #[test]
    fn exclusive_is_reentrant_for_same_owner() {
        let lock = Arc::new(TableLock::new());
        lock.acquire_exclusive(owner(3));
        lock.acquire_exclusive(owner(3)); // must not block
        lock.release_exclusive(owner(3));
    }

    #[test]
    fn exclusive_upgrades_over_own_shared_hold() {
        // The realistic #3 case: a session holds a shared hold (an open cursor)
        // and then TRUNCATEs the same table. It must not self-deadlock.
        let lock = Arc::new(TableLock::new());
        let _cursor = lock.acquire_shared(owner(7));
        lock.acquire_exclusive(owner(7)); // same owner: granted despite the shared hold
        assert_eq!(lock.inner.lock().unwrap().exclusive, Some(owner(7)));
        lock.release_exclusive(owner(7));
    }

    #[test]
    fn exclusive_waits_for_a_foreign_shared_hold() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        let lock = Arc::new(TableLock::new());
        // A DIFFERENT owner's shared hold must block the exclusive.
        let held = lock.acquire_shared(owner(3));
        let got_exclusive = Arc::new(AtomicBool::new(false));

        let l2 = Arc::clone(&lock);
        let flag = Arc::clone(&got_exclusive);
        let t = thread::spawn(move || {
            l2.acquire_exclusive(owner(4));
            flag.store(true, Ordering::SeqCst);
            l2.release_exclusive(owner(4));
        });

        thread::sleep(Duration::from_millis(50));
        assert!(
            !got_exclusive.load(Ordering::SeqCst),
            "exclusive must wait for another owner's shared hold"
        );
        drop(held);
        t.join().unwrap();
        assert!(got_exclusive.load(Ordering::SeqCst));
    }
}
