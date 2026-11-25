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

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use crate::config::crosscheck::reference_surfaces;
use crate::config::{Config, Resolved};
use crate::secrets::build_source;
use crate::template::{Resolver, Token, references};

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
    /// Any non-empty value is registered: a 1–3 byte default is still a
    /// secret and must not appear on the wire or in a log. Values shorter than
    /// 4 bytes are stored in their exact form only — their percent-encoded and
    /// base64 forms are short enough to occur inside unrelated output, so the
    /// extra variants would over-censor (§7.12). Three forms are stored for
    /// longer values — the exact value, its percent-encoded form
    /// (`NON_ALPHANUMERIC`) and its base64 form (standard alphabet) — plus the
    /// unpadded base64 variant when the base64 form ends with `=`.
    pub fn register(&self, name: &str, value: &str) {
        if value.is_empty() {
            return;
        }

        let mut needles = vec![value.to_string()];
        if value.len() >= 4 {
            needles.push(utf8_percent_encode(value, NON_ALPHANUMERIC).to_string());
            let encoded = STANDARD.encode(value);
            needles.push(encoded.clone());
            if encoded.ends_with('=') {
                needles.push(encoded.trim_end_matches('=').to_string());
            }
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

/// Seed a [`Redactor`] from what the config actually references, not only
/// from the declaration file (§7.12).
///
/// Three layers, in order: every `{{env.X}}` reached by a template in the
/// shared reference surface (operations and `users.yaml`), then every declared
/// name that resolves (parity with the declaration-driven behaviour), then
/// each user's static `token:` in its *resolved* form, which is the string
/// that actually goes on the wire.
///
/// Task 18 (`run`) and the TUI call this rather than hand-rolling the
/// seeding. A `Some(None)` (explicitly unset) or `None` (absent) user token
/// is skipped.
#[must_use]
pub fn redactor_for(config: &Config, resolved: &Resolved) -> Redactor {
    let redactor = Redactor::new();
    let source = build_source(&resolved.secrets, &config.root_dir);

    // A reference is redacted whether or not it is declared: a shell variable
    // is a real value either way, and this is the layer that hides it.
    for surface in reference_surfaces(config) {
        for reference in references(&surface.value) {
            if let Token::Env(name) = &reference.token
                && let Some(value) = source.lookup(name).value
            {
                redactor.register(name, &value);
            }
        }
    }

    if let Some(declaration) = &resolved.secrets.declaration {
        for name in declaration.0.keys() {
            if let Some(value) = source.lookup(name).value {
                redactor.register(name, &value);
            }
        }
    }

    let no_session = BTreeMap::new();
    let no_files = BTreeMap::new();
    let resolver = Resolver {
        vars: &resolved.vars,
        session: &no_session,
        env: source.as_ref(),
        files: &no_files,
    };
    for (user, spec) in &resolved.users {
        let Some(Some(token)) = &spec.token else {
            continue;
        };
        if let Ok(Value::String(value)) = resolver.resolve_str(token) {
            redactor.register(&format!("USER_{user}"), &value);
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
