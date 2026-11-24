//! Secret-source seam (§7.11).
//!
//! This module defines the shape every secret source must satisfy, but does
//! not read the environment or a dotenv file — Task 13 implements
//! `build_source` and the concrete sources against exactly this trait. The
//! templating layer only ever holds a `&dyn SecretSource`, so nothing here
//! can reach the process environment or the filesystem.

use std::path::PathBuf;

/// The result of looking up one secret name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// The secret value, or `None` when the name is not set.
    pub value: Option<String>,
    /// Where the value came from, when present.
    pub origin: Option<Origin>,
}

/// Where a secret value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Read from the process environment.
    Shell,
    /// Read from a pinned dotenv file.
    Dotenv(PathBuf),
    /// A harmless committed `default`.
    Default,
}

/// A source of secret values.
///
/// The defaulted `names()` lets `{{env.X}}` suggest a close name
/// (`API_TOKN` → `API_TOKEN`) when a lookup misses. Sources that cannot
/// enumerate names return the default empty list.
pub trait SecretSource {
    fn lookup(&self, name: &str) -> Lookup;

    fn names(&self) -> Vec<String> {
        Vec::new()
    }
}
