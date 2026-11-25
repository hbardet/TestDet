//! Per-user sessions (§7.7): one client, cookie jar and captured variable
//! table per user, created lazily and reused for the whole run.
//!
//! ```text
//! SessionStore::new(ctx)
//!   ensure(user)      validate → reuse → static token | login op | nothing
//!   run(user, op)     send through that user's client, capture nothing
//!   vars(user)        the user's captured {{session.*}} table
//!   capture_into(..)  parse capture: paths out of a response body
//! execute_for_user(store, user, op)   ensure → run → (auth failure) reset,
//!                                    ensure → run once more
//! ```
//!
//! Anonymous runs are a session keyed by `None`, so the engine keeps a single
//! `Option<String>` user throughout.

mod login;
mod store;

pub use login::{execute_for_user, is_auth_failure};
pub use store::{SessionStore, resolve_capture_path};
