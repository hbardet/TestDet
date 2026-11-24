//! TestDet core: configuration, engine, sessions, secrets and redaction.
//!
//! This crate never performs terminal I/O: it does not print, read stdin, or
//! inspect whether a file descriptor is a TTY. It returns data; the frontends
//! (`testdet-cli`, `testdet-tui`) own presentation.

/// The version of the TestDet distribution, read from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod diagnostic;
pub mod operation;
