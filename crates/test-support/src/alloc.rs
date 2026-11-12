//! A counting global allocator for benches and allocation tests.
//!
//! Install it in the binary that measures (a bench, or an integration-test
//! file), never in a library:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: agui_test_support::alloc::CountingAllocator =
//!     agui_test_support::alloc::CountingAllocator;
//!
//! let (value, stats) = agui_test_support::alloc::measure(|| work());
//! assert_eq!(stats.allocations, 0);
//! ```
//!
//! Counting is per thread and only while a [`measure`] call is active on that
//! thread, so tests running in parallel do not see each other's allocations.
//! Without the allocator installed, [`measure`] reports zeros and
//! [`installed`] returns `false`; callers that assert budgets check it first.

#![allow(unsafe_code)] // `GlobalAlloc` is an unsafe trait; this module only forwards to `System`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Allocation counts for one measured region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Calls to `alloc` and `alloc_zeroed`.
    pub allocations: u64,
    /// Calls to `realloc`.
    pub reallocations: u64,
    /// Calls to `dealloc`.
    pub deallocations: u64,
    /// Bytes requested by `alloc`, `alloc_zeroed`, and growth in `realloc`.
    pub bytes: u64,
    /// Bytes given back by `dealloc`, and shrinkage in `realloc`.
    pub freed: u64,
}

impl AllocStats {
    /// Allocations plus reallocations: the number budgets are written in.
    pub fn total(&self) -> u64 {
        self.allocations + self.reallocations
    }

    /// Requested bytes still held at the end of the region: what it
    /// retained (negative when it freed more than it allocated).
    pub fn live(&self) -> i64 {
        self.bytes as i64 - self.freed as i64
    }
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static REALLOCS: Cell<u64> = const { Cell::new(0) };
    static DEALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static FREED: Cell<u64> = const { Cell::new(0) };
    static SEEN: Cell<bool> = const { Cell::new(false) };
}

#[inline]
fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>, by: u64) {
    // `try_with`: during thread teardown the slots may be gone; skip then.
    let _ = ACTIVE.try_with(|active| {
        if active.get() {
            let _ = counter.try_with(|c| c.set(c.get() + by));
        }
    });
}

/// Forwards to [`System`], counting calls on the current thread while a
/// [`measure`] is active.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountingAllocator;

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged; the counters are const-initialised thread-locals of `Cell<u64>`
// and `Cell<bool>`, which never allocate and have no destructor.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = SEEN.try_with(|s| s.set(true));
        bump(&ALLOCS, 1);
        bump(&BYTES, layout.size() as u64);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = SEEN.try_with(|s| s.set(true));
        bump(&ALLOCS, 1);
        bump(&BYTES, layout.size() as u64);
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(&DEALLOCS, 1);
        bump(&FREED, layout.size() as u64);
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(&REALLOCS, 1);
        bump(&BYTES, new_size.saturating_sub(layout.size()) as u64);
        bump(&FREED, layout.size().saturating_sub(new_size) as u64);
        System.realloc(ptr, layout, new_size)
    }
}

fn read() -> AllocStats {
    AllocStats {
        allocations: ALLOCS.with(Cell::get),
        reallocations: REALLOCS.with(Cell::get),
        deallocations: DEALLOCS.with(Cell::get),
        bytes: BYTES.with(Cell::get),
        freed: FREED.with(Cell::get),
    }
}

/// Whether [`CountingAllocator`] is the global allocator of this binary
/// (detected by allocating once and looking for the call).
pub fn installed() -> bool {
    SEEN.with(|s| s.set(false));
    drop(std::hint::black_box(Box::new(0u64)));
    SEEN.with(Cell::get)
}

/// Runs `f` and returns its result with the allocations it made on this
/// thread. Calls may nest: each returns the allocations made during its own
/// closure, and an outer call includes those of the calls nested in it.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    let was_active = ACTIVE.with(|a| a.replace(true));
    let before = read();
    let result = f();
    let after = read();
    ACTIVE.with(|a| a.set(was_active));
    (
        result,
        AllocStats {
            allocations: after.allocations - before.allocations,
            reallocations: after.reallocations - before.reallocations,
            deallocations: after.deallocations - before.deallocations,
            bytes: after.bytes - before.bytes,
            freed: after.freed - before.freed,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: CountingAllocator = CountingAllocator;

    #[test]
    fn counts_this_thread_only_while_measuring() {
        assert!(installed());
        let (len, stats) = measure(|| {
            let mut v: Vec<u8> = Vec::with_capacity(8);
            v.extend_from_slice(&[0; 64]);
            v.len()
        });
        assert_eq!(len, 64);
        assert_eq!(stats.allocations, 1);
        assert!(stats.reallocations >= 1);
        assert_eq!(stats.deallocations, 1);
        assert!(stats.bytes >= 64);
        let (_, idle) = measure(|| std::hint::black_box(1 + 1));
        assert_eq!(idle, AllocStats::default());
        // Another thread's allocations are not counted here.
        let (_, other) = measure(|| {
            std::thread::spawn(|| drop(std::hint::black_box(vec![0u8; 1024])))
                .join()
                .unwrap_or_else(|_| panic!("thread"));
        });
        assert!(other.bytes < 1024, "{other:?}");
    }
}
