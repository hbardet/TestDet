//! Live redaction registry (§7.12).
//!
//! [`Redactor`] holds a live, interior-mutable set of secret patterns: seeded
//! from the resolved `{{env.*}}` values and user `token:` headers, grown later
//! as sessions capture tokens. Every frontend scrubs through the same
//! registry, so a token captured at run time is as hidden in logs as one from
//! an env var. Matches are replaced with `<NAME>-CENSORED`; the value is
//! never revealed.
//!
//! The registry is read-mostly, so it uses [`RwLock`]; reads recover from a
//! poisoned lock rather than panicking, and no read guard is held across an
//! await (there is no async here).

use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use crate::config::{Config, Resolved};
use crate::secrets::build_source;

/// A live, interior-mutable set of secret patterns.
///
/// [`register`](Redactor::register) may be called at any time; cloning shares
/// the same registry, which is how `redact` and `session` agree on one
/// pattern set (§7.12).
#[derive(Clone)]
pub struct Redactor(Arc<RwLock<Inner>>);

#[derive(Default)]
struct Inner {
    patterns: Vec<Pattern>,
}

/// One pattern: the `name` used in the `<NAME>-CENSORED` tag and the exact
/// `needle` text to replace.
struct Pattern {
    name: String,
    needle: String,
}

impl Redactor {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one secret value under `name`.
    ///
    /// Empty values and values shorter than 4 bytes are ignored: a 1-byte
    /// "secret" would censor half of any output it appeared in, so the
    /// threshold trades a vanishingly rare leak for no false positives.
    ///
    /// Three forms are stored — the exact value, its percent-encoded form
    /// (`NON_ALPHANUMERIC`) and its base64 form (standard alphabet) — plus the
    /// unpadded base64 variant when the base64 form ends with `=`.
    pub fn register(&self, name: &str, value: &str) {
        if value.is_empty() || value.len() < 4 {
            return;
        }

        let mut needles = vec![value.to_string()];
        needles.push(utf8_percent_encode(value, NON_ALPHANUMERIC).to_string());
        let encoded = STANDARD.encode(value);
        needles.push(encoded.clone());
        if encoded.ends_with('=') {
            needles.push(encoded.trim_end_matches('=').to_string());
        }

        let mut inner = write_lock(&self.0);
        for needle in needles {
            if inner
                .patterns
                .iter()
                .any(|pattern| pattern.needle == needle)
            {
                continue;
            }
            inner.patterns.push(Pattern {
                name: name.to_string(),
                needle,
            });
        }
    }

    /// Replace every registered secret in `s` with `<NAME>-CENSORED`.
    ///
    /// Patterns are applied longest-first so a value that contains a shorter
    /// registered value is replaced wholesale. Scrubbing is idempotent: the
    /// replacement tag contains no registered needle (unless a needle is
    /// itself a substring of the tag, which registration makes impossible in
    /// practice).
    #[must_use]
    pub fn scrub(&self, s: &str) -> String {
        let guard = read_lock(&self.0);
        let mut patterns: Vec<&Pattern> = guard.patterns.iter().collect();
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.needle.len()));

        let mut out = s.to_string();
        for pattern in patterns {
            out = out.replace(&pattern.needle, &replacement_tag(&pattern.name));
        }
        out
    }

    /// Scrub every string value in a JSON document, in place.
    ///
    /// Objects are walked value-by-value (keys are left alone — a capture path
    /// may legitimately contain a secret name) and arrays element-by-element.
    pub fn scrub_json(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.scrub(s),
            Value::Array(items) => {
                for item in items {
                    self.scrub_json(item);
                }
            }
            Value::Object(map) => {
                for child in map.values_mut() {
                    self.scrub_json(child);
                }
            }
            _ => {}
        }
    }

    /// The registered names, in first-registration order, deduplicated.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let guard = read_lock(&self.0);
        let mut names = Vec::new();
        for pattern in &guard.patterns {
            if !names.contains(&pattern.name) {
                names.push(pattern.name.clone());
            }
        }
        names
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(Inner::default())))
    }
}

/// Seed a [`Redactor`] from a resolved config: every declared secret that
/// resolves to a value, and every user's set token (as `USER_<user>`).
///
/// Task 18 (`run`) and the TUI call this rather than hand-rolling the
/// seeding. A `Some(None)` (explicitly unset) or `None` (absent) user token
/// is skipped.
#[must_use]
pub fn redactor_for(config: &Config, resolved: &Resolved) -> Redactor {
    let redactor = Redactor::new();
    let source = build_source(&resolved.secrets, &config.root_dir);

    if let Some(declaration) = &resolved.secrets.declaration {
        for name in declaration.0.keys() {
            if let Some(value) = source.lookup(name).value {
                redactor.register(name, &value);
            }
        }
    }

    for (user, spec) in &resolved.users {
        if let Some(Some(token)) = &spec.token {
            redactor.register(&format!("USER_{user}"), token);
        }
    }

    redactor
}

/// The `<NAME>-CENSORED` replacement tag for a name.
///
/// The name is kept verbatim, except that an all-uppercase name (the spec's
/// `API_TOKEN`) is uppercased — a no-op that documents the intent while
/// preserving a user-token name like `USER_alice`.
fn replacement_tag(name: &str) -> String {
    let tag = if name.bytes().any(|b| b.is_ascii_lowercase()) {
        name.to_string()
    } else {
        name.to_ascii_uppercase()
    };
    format!("<{tag}>-CENSORED")
}

/// Recover from a poisoned read lock by using the current contents.
fn read_lock(lock: &RwLock<Inner>) -> std::sync::RwLockReadGuard<'_, Inner> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Recover from a poisoned write lock by using the current contents.
fn write_lock(lock: &RwLock<Inner>) -> std::sync::RwLockWriteGuard<'_, Inner> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
