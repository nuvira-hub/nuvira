//! Macro benchmarks (plan 7.2): `nuvira-bench run macro`.
//!
//! - [`composed`] (WP3): B-MACRO-01 (composed pipeline, stream), B-MACRO-02
//!   (datagrams + FEC, through the netem proxy), B-MACRO-08a (input transport latency);
//! - [`session`] (WP3): B-MACRO-03 (real agent session in loopback, synthetic
//!   source, headless viewer), B-MACRO-04 (the same through the netem proxy, ABR);
//! - `connect`, `reconnect`, `files`, `input` (WP4): B-MACRO-05..07, 08b, 10.
//!
//! Each file provides `pub fn register(reg: &mut Registry)`; WP4's files are
//! declared only when present (`build.rs` cfgs), so either package builds alone.
//! Every macro case of WP3 runs in a child process of its own (`isolated()`), as
//! the plan wants one fresh process per session variant.

pub mod composed;
pub mod session;

#[cfg(bench_has_macro__connect)]
pub mod connect;
#[cfg(bench_has_macro__files)]
pub mod files;
#[cfg(bench_has_macro__input)]
pub mod input;
#[cfg(bench_has_macro__reconnect)]
pub mod reconnect;

use crate::registry::Registry;

pub fn register(reg: &mut Registry) {
    composed::register(reg);
    session::register(reg);
    #[cfg(bench_has_macro__connect)]
    connect::register(reg);
    #[cfg(bench_has_macro__reconnect)]
    reconnect::register(reg);
    #[cfg(bench_has_macro__input)]
    input::register(reg);
    #[cfg(bench_has_macro__files)]
    files::register(reg);
}
