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
pub mod files;
pub mod operation;
pub mod outcome;
pub mod redact;
pub mod secrets;
pub mod session;
pub mod template;

/// Re-exported because the boundary types already carry byte and JSON values
/// (`Outcome::body`, `GraphQLError::extensions`, `Resolved::vars`): a frontend
/// must be able to build, serialise and render them without adding its own
/// dependency on the same crates (§7.9, §7.16).
pub use bytes::Bytes;
pub use context::RunContext;
pub use engine::multipart::{build_graphql_multipart, build_rest_multipart};
pub use engine::{EnvelopeShape, PreparedRequest, execute, prepare, read_envelope};
pub use events::{Event, EventSink};
pub use files::{FileCache, LoadedFile, check_path, sniff_mime};
pub use outcome::{
    CookieInfo, GraphQLError, Outcome, RequestEcho, Timing, TransportError, TransportKind,
};
pub use serde_json;
pub use session::{SessionStore, execute_for_user, is_auth_failure, resolve_capture_path};
