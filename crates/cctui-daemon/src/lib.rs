//! cctui-daemon: per-machine agent supervisor.
//!
//! Authenticates to a cctui-server with a long-lived machine key, opens a
//! bidirectional WS, instantiates compiled-in adapter modules, and bridges
//! adapter events ↔ server frames.
//!
//! See `crates/cctui-daemon/src/main.rs` for the CLI entry point.

pub mod adapter_runtime;
pub mod adapters;
pub mod agenttool;
pub mod askhook;
pub mod blobs;
pub mod bus;
pub mod childenv;
pub mod childwatch;
pub mod client;
pub mod config;
pub mod configsweep;
pub mod counters;
pub mod dispatch_codex;
pub mod enroll;
pub mod fatal;
pub mod git;
pub mod imagepost;
pub mod listdirs;
pub mod mcp;
pub mod mempressure;
pub mod offsets;
pub mod readfile;
pub mod resources;
pub mod runlock;
pub mod runtime;
pub mod selfupdate;
pub mod sendguard;
pub mod service;
pub mod supervisor;
pub mod updatehook;
pub mod whipstop;
