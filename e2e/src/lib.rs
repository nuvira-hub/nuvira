//! End-to-end characterisation tests of whole NuviraHub sessions on 127.0.0.1
//! (full review, Phase 2; `nuvira-pro/docs/review/features.md`, gap 1).
//!
//! The parts are the real ones, through their public APIs: the agent
//! (`nuvira_agent::serve_reverse`, `serve_direct`, `serve_via_server`) on the
//! synthetic source, a real `nuvira-server` process built from `../server`, and
//! the bench's headless viewer (`nuvira_bench::net::headless`), netem proxy and
//! local-server helpers (`nuvira_bench::net`). Nothing touches the real screen,
//! the OS input or the OS clipboard, except the Xvfb group (Linux only, a virtual
//! X server started for the run: see `tests/e2e/xvfb.rs`).
//!
//! - [`runner`]: the test runner of `tests/e2e/main.rs` (`harness = false`): each
//!   case runs in a child process of its own, with its own data folder and
//!   environment.
//! - [`rig`]: what the cases share: runtimes, an agent registered on the local
//!   server with its events and commands, the viewer's way through signalling,
//!   a raw control channel for the handshake cases, helpers to wait and to read
//!   close reasons.

pub mod rig;
pub mod runner;
