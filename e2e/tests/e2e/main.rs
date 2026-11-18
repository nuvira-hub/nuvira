//! End-to-end session tests on 127.0.0.1 (see `../../README.md` and
//! `nuvira_e2e::runner`). `cargo test --release` runs the default group; each
//! case is a child process of this binary.

mod hosts;
mod loopback;
mod server;
mod xvfb;

use nuvira_e2e::runner::{self, Case};

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    v.extend(loopback::cases());
    v.extend(server::cases());
    v.extend(xvfb::cases());
    v
}

fn main() {
    std::process::exit(runner::main(&cases(), hosts::dispatch));
}
