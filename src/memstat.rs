//! Process memory measurement, for comparing implementations across the
//! FFI boundary.
//!
//! # Why not just a Rust allocator hook
//!
//! A `GlobalAlloc` wrapper counts Rust allocations precisely and misses
//! every `malloc` that yyjson and simdjson make — which is the entire point
//! of the comparison. So two sources are used together:
//!
//! * **`mstats()`** (macOS) / **`mallinfo2()`** (glibc) reports the live
//!   heap of the *system allocator*. Rust's default `System` allocator is
//!   `malloc` on both platforms, so this single number covers Rust and C
//!   alike. This is the figure to quote for "how much memory does the
//!   parsed form occupy".
//! * **[`Counter`]**, an optional `GlobalAlloc` wrapper, gives allocation
//!   *counts* — which no heap-size number can show, and which is what
//!   separates "one 10 MB buffer" from "a million 10-byte nodes".
//!
//! **`getrusage(RUSAGE_SELF).ru_maxrss`** provides the peak RSS as a
//! cross-check, since it captures anything the heap counters miss (mmap,
//! stacks, the binary itself).
//!
//! # Caveats
//!
//! * Peak RSS is monotonic per process and never falls, so it is only
//!   meaningful as a whole-process figure — hence one process per
//!   measurement (`tools/memprofile.sh`).
//! * `mstats().bytes_used` includes allocator bookkeeping and rounding, so
//!   it slightly overstates the payload. It is consistent between
//!   implementations, which is what matters here.
//! * Nothing here is a substitute for a real heap profiler on a real
//!   workload; it is a comparison instrument.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

// --- system heap ------------------------------------------------------

#[cfg(target_os = "macos")]
mod sys {
    /// `struct mstats` from `<malloc/malloc.h>`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MStats {
        bytes_total: usize,
        chunks_used: usize,
        bytes_used: usize,
        chunks_free: usize,
        bytes_free: usize,
    }

    extern "C" {
        fn mstats() -> MStats;
    }

    /// Live bytes in the system heap.
    pub fn heap_in_use() -> usize {
        // SAFETY: `mstats` takes no arguments and returns a POD struct by
        // value; the layout above matches `<malloc/malloc.h>`.
        unsafe { mstats() }.bytes_used
    }

    /// Bytes the allocator has obtained from the OS.
    pub fn heap_reserved() -> usize {
        // SAFETY: as above.
        unsafe { mstats() }.bytes_total
    }

    /// Number of live allocations, as the allocator sees them.
    pub fn heap_chunks() -> usize {
        // SAFETY: as above.
        unsafe { mstats() }.chunks_used
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod sys {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MallInfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }

    extern "C" {
        fn mallinfo2() -> MallInfo2;
    }

    pub fn heap_in_use() -> usize {
        // SAFETY: POD return, layout matches glibc's <malloc.h>.
        unsafe { mallinfo2() }.uordblks
    }
    pub fn heap_reserved() -> usize {
        // SAFETY: as above.
        let m = unsafe { mallinfo2() };
        m.arena + m.hblkhd
    }
    pub fn heap_chunks() -> usize {
        // SAFETY: as above.
        unsafe { mallinfo2() }.ordblks
    }
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
mod sys {
    pub fn heap_in_use() -> usize {
        0
    }
    pub fn heap_reserved() -> usize {
        0
    }
    pub fn heap_chunks() -> usize {
        0
    }
}

/// Is a system-heap reading available on this platform?
#[must_use]
pub fn heap_supported() -> bool {
    cfg!(any(
        target_os = "macos",
        all(target_os = "linux", target_env = "gnu")
    ))
}

// --- resident set -----------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    sec: i64,
    usec: i64,
}

/// `struct rusage`. Only `ru_maxrss` is read; the rest is padding to the
/// right size so the kernel does not write past the struct.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RUsage {
    utime: Timeval,
    stime: Timeval,
    maxrss: i64,
    rest: [i64; 14],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

/// Peak resident set size, in bytes.
///
/// Monotonic for the life of the process — it never decreases — so it is
/// only meaningful as a whole-process number.
#[must_use]
pub fn rss_peak() -> usize {
    let mut u = RUsage::default();
    // SAFETY: `getrusage` writes at most `sizeof(struct rusage)` bytes into
    // the pointer. `RUsage` is 18 `i64`s = 144 bytes, which matches or
    // exceeds the struct on both macOS and Linux.
    let rc = unsafe {
        getrusage(0 /* RUSAGE_SELF */, &mut u)
    };
    if rc != 0 {
        return 0;
    }
    let v = u.maxrss.max(0) as usize;
    // macOS reports bytes; Linux reports kilobytes.
    if cfg!(target_os = "linux") {
        v * 1024
    } else {
        v
    }
}

// --- Rust-side allocation counter -------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static LIVE_PEAK: AtomicUsize = AtomicUsize::new(0);

/// A `GlobalAlloc` wrapper that counts Rust allocations.
///
/// Install in a measurement binary only:
///
/// ```ignore
/// #[global_allocator]
/// static A: dacode_json::memstat::Counter = dacode_json::memstat::Counter;
/// ```
///
/// The counters are `Relaxed` atomics — a few nanoseconds per allocation,
/// which is visible in a tight allocation loop. Do not install it in a
/// binary you are also timing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counter;

// SAFETY: every method forwards to `System` unchanged; the counters do not
// affect the returned pointers or the layouts.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim to the system allocator.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            LIVE_PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr`/`layout` come from a matching `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded verbatim.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
            let live = LIVE
                .fetch_add(new_size.wrapping_sub(layout.size()), Ordering::Relaxed)
                .wrapping_add(new_size.wrapping_sub(layout.size()));
            LIVE_PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
}

/// Rust allocations since process start, per [`Counter`].
///
/// [`Snapshot::now`] reports the same number alongside everything else,
/// but it also queries the system allocator. This reads one atomic and
/// nothing more, so it can bracket a region that is asserted to allocate
/// zero times without the measurement perturbing the count.
#[must_use]
pub fn rust_allocs() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

/// A point-in-time memory reading.
#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    /// Live bytes in the system heap (Rust **and** C).
    pub heap_in_use: usize,
    /// Bytes the allocator holds from the OS.
    pub heap_reserved: usize,
    /// Live allocation count, per the allocator.
    pub heap_chunks: usize,
    /// Peak RSS so far. Monotonic.
    pub rss_peak: usize,
    /// Rust allocations since process start ([`Counter`] only).
    pub rust_allocs: usize,
    /// Rust bytes requested since process start ([`Counter`] only).
    pub rust_bytes: usize,
    /// Live Rust bytes ([`Counter`] only).
    pub rust_live: usize,
    /// Peak live Rust bytes ([`Counter`] only).
    pub rust_live_peak: usize,
}

impl Snapshot {
    /// Read the current state.
    #[must_use]
    pub fn now() -> Self {
        Snapshot {
            heap_in_use: sys::heap_in_use(),
            heap_reserved: sys::heap_reserved(),
            heap_chunks: sys::heap_chunks(),
            rss_peak: rss_peak(),
            rust_allocs: ALLOCS.load(Ordering::Relaxed),
            rust_bytes: BYTES.load(Ordering::Relaxed),
            rust_live: LIVE.load(Ordering::Relaxed),
            rust_live_peak: LIVE_PEAK.load(Ordering::Relaxed),
        }
    }

    /// `self - earlier`, saturating so a shrinking heap reads as 0 rather
    /// than wrapping.
    #[must_use]
    pub fn since(&self, earlier: &Snapshot) -> Delta {
        Delta {
            heap_in_use: self.heap_in_use as i64 - earlier.heap_in_use as i64,
            heap_reserved: self.heap_reserved as i64 - earlier.heap_reserved as i64,
            heap_chunks: self.heap_chunks as i64 - earlier.heap_chunks as i64,
            rss_peak: self.rss_peak.saturating_sub(earlier.rss_peak),
            rust_allocs: self.rust_allocs.saturating_sub(earlier.rust_allocs),
            rust_bytes: self.rust_bytes.saturating_sub(earlier.rust_bytes),
            rust_live: self.rust_live as i64 - earlier.rust_live as i64,
            rust_live_peak: self.rust_live_peak.saturating_sub(earlier.rust_live_peak),
        }
    }
}

/// The difference between two [`Snapshot`]s.
///
/// Heap figures are signed: freeing more than was allocated is normal when
/// a measurement window ends with a teardown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Delta {
    pub heap_in_use: i64,
    pub heap_reserved: i64,
    pub heap_chunks: i64,
    pub rss_peak: usize,
    pub rust_allocs: usize,
    pub rust_bytes: usize,
    pub rust_live: i64,
    pub rust_live_peak: usize,
}

/// Format bytes as a human-readable string.
#[must_use]
pub fn human(bytes: i64) -> String {
    let neg = bytes < 0;
    let b = bytes.unsigned_abs() as f64;
    let (v, unit) = if b >= 1024.0 * 1024.0 * 1024.0 {
        (b / (1024.0 * 1024.0 * 1024.0), "GiB")
    } else if b >= 1024.0 * 1024.0 {
        (b / (1024.0 * 1024.0), "MiB")
    } else if b >= 1024.0 {
        (b / 1024.0, "KiB")
    } else {
        (b, "B")
    };
    format!("{}{v:.2} {unit}", if neg { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_tracks_a_large_allocation() {
        if !heap_supported() {
            return;
        }
        let before = Snapshot::now();
        let v: Vec<u8> = vec![0u8; 32 << 20];
        let after = Snapshot::now();
        let d = after.since(&before);
        // Should see most of 32 MiB. Allow slack for allocator behaviour.
        assert!(
            d.heap_in_use > 24 << 20,
            "expected ~32 MiB, saw {}",
            human(d.heap_in_use)
        );
        drop(v);
        let freed = Snapshot::now().since(&after);
        assert!(
            freed.heap_in_use < 0,
            "freeing should reduce the live heap, saw {}",
            human(freed.heap_in_use)
        );
    }

    #[test]
    fn rss_peak_is_monotonic() {
        let a = rss_peak();
        let v: Vec<u8> = vec![1u8; 8 << 20];
        std::hint::black_box(&v);
        let b = rss_peak();
        assert!(b >= a, "peak RSS went down: {a} -> {b}");
    }

    #[test]
    fn human_formats() {
        assert_eq!(human(0), "0.00 B");
        assert_eq!(human(1024), "1.00 KiB");
        assert_eq!(human(1024 * 1024), "1.00 MiB");
        assert_eq!(human(-2 * 1024 * 1024), "-2.00 MiB");
    }
}
