//! Counting global allocator (plan section 6.2).
//!
//! Wraps the system allocator and counts allocations, frees, bytes and live bytes:
//! - process-wide per **side**: each thread carries a side marker (default
//!   [`Side::Agent`]; the bench marks the threads of its own viewer side with
//!   [`set_side`]`(Side::Viewer)`, e.g. in tokio's `on_thread_start`);
//! - per **thread**: exact counts for single-threaded micro measurements
//!   ([`ThreadAllocs::take`]), unaffected by other threads.
//!
//! The allocator path never allocates: atomics plus `const`-initialised
//! thread-local `Cell`s (no destructor, no lazy initialisation).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

/// Which side of an in-process session a thread belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Default: the agent runtime and every production thread.
    Agent = 0,
    /// Threads the bench creates for its own viewer side.
    Viewer = 1,
}

const SIDES: usize = 2;

struct Counters {
    allocs: AtomicU64,
    frees: AtomicU64,
    bytes: AtomicU64,
    freed_bytes: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            allocs: AtomicU64::new(0),
            frees: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            freed_bytes: AtomicU64::new(0),
        }
    }
}

static SIDE_COUNTERS: [Counters; SIDES] = [Counters::new(), Counters::new()];
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

thread_local! {
    static SIDE: Cell<u8> = const { Cell::new(0) };
    static T_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static T_FREES: Cell<u64> = const { Cell::new(0) };
    static T_BYTES: Cell<u64> = const { Cell::new(0) };
}

/// The allocator installed by `lib.rs` as `#[global_allocator]`.
pub struct CountingAlloc;

#[inline]
fn on_alloc(size: usize) {
    let side = SIDE.try_with(|s| s.get()).unwrap_or(0) as usize;
    let c = &SIDE_COUNTERS[side.min(SIDES - 1)];
    c.allocs.fetch_add(1, Relaxed);
    c.bytes.fetch_add(size as u64, Relaxed);
    let live = LIVE.fetch_add(size as i64, Relaxed) + size as i64;
    PEAK.fetch_max(live, Relaxed);
    let _ = T_ALLOCS.try_with(|n| n.set(n.get() + 1));
    let _ = T_BYTES.try_with(|n| n.set(n.get() + size as u64));
}

#[inline]
fn on_free(size: usize) {
    let side = SIDE.try_with(|s| s.get()).unwrap_or(0) as usize;
    let c = &SIDE_COUNTERS[side.min(SIDES - 1)];
    c.frees.fetch_add(1, Relaxed);
    c.freed_bytes.fetch_add(size as u64, Relaxed);
    LIVE.fetch_sub(size as i64, Relaxed);
    let _ = T_FREES.try_with(|n| n.set(n.get() + 1));
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            on_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            on_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        on_free(layout.size());
    }

    /// A reallocation counts as one allocation of the new size and one free of
    /// the old one (it may copy the whole buffer).
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            on_free(layout.size());
            on_alloc(new_size);
        }
        p
    }
}

/// Marks the calling thread as belonging to `side`.
pub fn set_side(side: Side) {
    SIDE.with(|s| s.set(side as u8));
}

/// Side of the calling thread.
pub fn current_side() -> Side {
    if SIDE.with(|s| s.get()) == Side::Viewer as u8 { Side::Viewer } else { Side::Agent }
}

/// For `tokio::runtime::Builder::on_thread_start` of the bench's viewer runtime.
pub fn mark_viewer_thread() {
    set_side(Side::Viewer);
}

/// Counts of one side (or of one thread) at a moment, or a difference of two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct AllocCounts {
    pub allocs: u64,
    pub frees: u64,
    pub bytes: u64,
    pub freed_bytes: u64,
}

impl AllocCounts {
    /// `self - earlier` (counters only grow).
    pub fn since(&self, earlier: &AllocCounts) -> AllocCounts {
        AllocCounts {
            allocs: self.allocs.wrapping_sub(earlier.allocs),
            frees: self.frees.wrapping_sub(earlier.frees),
            bytes: self.bytes.wrapping_sub(earlier.bytes),
            freed_bytes: self.freed_bytes.wrapping_sub(earlier.freed_bytes),
        }
    }
}

/// Process-wide counters per side plus live and peak live bytes.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct AllocSnapshot {
    pub agent: AllocCounts,
    pub viewer: AllocCounts,
    /// Bytes allocated and not yet freed (all sides).
    pub live_bytes: i64,
    /// Highest `live_bytes` since start or since the last [`reset_peak`].
    pub peak_live_bytes: i64,
}

impl AllocSnapshot {
    pub fn take() -> Self {
        let read = |c: &Counters| AllocCounts {
            allocs: c.allocs.load(Relaxed),
            frees: c.frees.load(Relaxed),
            bytes: c.bytes.load(Relaxed),
            freed_bytes: c.freed_bytes.load(Relaxed),
        };
        AllocSnapshot {
            agent: read(&SIDE_COUNTERS[0]),
            viewer: read(&SIDE_COUNTERS[1]),
            live_bytes: LIVE.load(Relaxed),
            peak_live_bytes: PEAK.load(Relaxed),
        }
    }

    /// Counts of `side`.
    pub fn side(&self, side: Side) -> AllocCounts {
        match side {
            Side::Agent => self.agent,
            Side::Viewer => self.viewer,
        }
    }

    /// Both sides together.
    pub fn total(&self) -> AllocCounts {
        AllocCounts {
            allocs: self.agent.allocs + self.viewer.allocs,
            frees: self.agent.frees + self.viewer.frees,
            bytes: self.agent.bytes + self.viewer.bytes,
            freed_bytes: self.agent.freed_bytes + self.viewer.freed_bytes,
        }
    }

    /// Per-side differences `self - earlier`; live/peak are those of `self`.
    pub fn since(&self, earlier: &AllocSnapshot) -> AllocSnapshot {
        AllocSnapshot {
            agent: self.agent.since(&earlier.agent),
            viewer: self.viewer.since(&earlier.viewer),
            live_bytes: self.live_bytes,
            peak_live_bytes: self.peak_live_bytes,
        }
    }
}

/// Sets the peak to the current live bytes (start of a measured region).
pub fn reset_peak() {
    PEAK.store(LIVE.load(Relaxed), Relaxed);
}

/// Exact counters of the calling thread.
pub struct ThreadAllocs;

impl ThreadAllocs {
    pub fn take() -> AllocCounts {
        AllocCounts {
            allocs: T_ALLOCS.with(|n| n.get()),
            frees: T_FREES.with(|n| n.get()),
            bytes: T_BYTES.with(|n| n.get()),
            freed_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_this_thread_exactly() {
        let before = ThreadAllocs::take();
        let v: Vec<u8> = Vec::with_capacity(4096);
        std::hint::black_box(&v);
        drop(v);
        let d = ThreadAllocs::take().since(&before);
        assert_eq!(d.allocs, 1);
        assert_eq!(d.bytes, 4096);
        assert_eq!(d.frees, 1);
    }

    #[test]
    fn side_marker_attributes_allocations() {
        std::thread::spawn(|| {
            set_side(Side::Viewer);
            assert_eq!(current_side(), Side::Viewer);
            let before = AllocSnapshot::take();
            let v = std::hint::black_box(vec![1u8; 1 << 20]);
            let after = AllocSnapshot::take();
            drop(v);
            assert!(after.since(&before).viewer.bytes >= 1 << 20);
        })
        .join()
        .unwrap();
        assert_eq!(current_side(), Side::Agent);
    }
}
