// crates/velocut-media/src/worker/semaphore.rs
//
// Generic semaphore guard for concurrency-limiting across threads.
// Deduplicates the identical SemGuard/G helpers that previously appeared
// in probe_clip, request_frame_hq, and request_transition_frame_hq.

use std::sync::{Arc, Condvar, Mutex};

/// RAII guard that decrements a shared semaphore counter on drop.
///
/// The semaphore is an `Arc<(Mutex<u32>, Condvar)>`:
/// - `Mutex<u32>` — the current count of active operations
/// - `Condvar` — used to wake waiters when the count decreases
pub(super) struct SemaphoreGuard {
    inner: Arc<(Mutex<u32>, Condvar)>,
}

impl SemaphoreGuard {
    /// Acquire the semaphore, blocking until `count < limit`.
    /// Increments the count and returns a guard that decrements on drop.
    pub fn acquire(sem: Arc<(Mutex<u32>, Condvar)>, limit: u32) -> Self {
        let (lock, cvar) = &*sem;
        let mut count = lock.lock().unwrap();
        while *count >= limit {
            count = cvar.wait(count).unwrap();
        }
        *count += 1;
        drop(count);
        SemaphoreGuard { inner: sem }
    }
}

impl Drop for SemaphoreGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.inner;
        *lock.lock().unwrap() -= 1;
        cvar.notify_one();
    }
}
