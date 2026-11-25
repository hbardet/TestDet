//! TestDet core: configuration, engine, sessions, secrets and redaction.
//!
//! This crate never performs terminal I/O: it does not print, read stdin, or
//! inspect whether a file descriptor is a TTY. It returns data; the frontends
//! (`testdet-cli`, `testdet-tui`) own presentation.

/// The version of the TestDet distribution, read from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod context;
pub mod diagnostic;
pub mod engine;
pub mod events;
pub mod operation;
pub mod outcome;
pub mod redact;
pub mod secrets;
pub mod session;
pub mod template;

pub use context::RunContext;
pub use engine::{EnvelopeShape, PreparedRequest, execute, prepare, read_envelope};
pub use events::{Event, EventSink};
pub use outcome::{
    CookieInfo, GraphQLError, Outcome, RequestEcho, Timing, TransportError, TransportKind,
};
pub use session::{SessionStore, execute_for_user, is_auth_failure, resolve_capture_path};
