//! Counters of the session in progress (one at a time), for the report to the server.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub static FILES_IN: AtomicU32 = AtomicU32::new(0);
pub static FILES_OUT: AtomicU32 = AtomicU32::new(0);
pub static BYTES_IN: AtomicU64 = AtomicU64::new(0);
pub static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

pub fn reset() {
    for c in [&FILES_IN, &FILES_OUT] {
        c.store(0, Ordering::Relaxed);
    }
    for c in [&BYTES_IN, &BYTES_OUT] {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn file_in(bytes: u64) {
    FILES_IN.fetch_add(1, Ordering::Relaxed);
    BYTES_IN.fetch_add(bytes, Ordering::Relaxed);
}

pub fn file_out(bytes: u64) {
    FILES_OUT.fetch_add(1, Ordering::Relaxed);
    BYTES_OUT.fetch_add(bytes, Ordering::Relaxed);
}
