//! Variables the user stored out of a response with `s`.
//!
//! A [`VarStore`] is **session-scoped and in memory only**. It is never
//! serialised, never written to the config directory or the state directory,
//! and it dies with the process (§7.16). Storing a token from a response must
//! not leave that token on disk.
//!
//! Values stored here are the top layer of the variable stack — above
//! operation, environment, user and `--var` values (§7.19):
//!
//! ```text
//! operation vars < env vars < user vars < --var < stored vars
//! ```

use std::collections::BTreeMap;

use testdet_core::serde_json::Value;

/// One stored variable: the value, plus where the user picked it up from.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    /// The captured JSON value, stored verbatim (no redaction: it came from
    /// the already-redacted outcome).
    pub value: Value,
    /// Human-readable provenance, e.g. `listProducts response`. Shown next to
    /// the value so a stale entry can be identified.
    pub source: String,
}

/// The in-memory store behind `s`.
#[derive(Debug, Clone, Default)]
pub struct VarStore {
    entries: BTreeMap<String, Stored>,
}

impl VarStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value` under `name`, replacing any earlier value of that name.
    pub fn store(&mut self, name: &str, value: Value, source: String) {
        self.entries
            .insert(name.to_string(), Stored { value, source });
    }

    /// The stored variables as the override map a run would merge last.
    ///
    /// This is the top layer described in the module docs; a caller layers it
    /// over [`testdet_core::config::Resolved::vars_for`] and the stored value wins.
    pub fn as_overrides(&self) -> BTreeMap<String, Value> {
        self.entries
            .iter()
            .map(|(name, stored)| (name.clone(), stored.value.clone()))
            .collect()
    }

    /// One stored value, for asserting precedence and for drawing.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries.get(name).map(|stored| &stored.value)
    }

    /// One stored entry including its provenance.
    pub fn get_stored(&self, name: &str) -> Option<&Stored> {
        self.entries.get(name)
    }

    /// Every entry, ordered by name.
    pub fn entries(&self) -> &BTreeMap<String, Stored> {
        &self.entries
    }

    /// Number of stored variables.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been stored yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
