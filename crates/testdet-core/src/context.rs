//! The `RunContext` boundary type: everything a single operation run needs.
//!
//! `RunContext` bundles the resolved environment, the selected user, the body
//! retention flag, the timeout, the shared redactor and the cancellation
//! token, plus the optional event sink. It is the "env, user, var overrides,
//! cancel token" seam of §7.3: one value the engine and every frontend can
//! construct and pass around.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Resolved;
use crate::events::{Event, EventSink};
use crate::files::FileCache;
use crate::redact::Redactor;

/// The immutable inputs and shared handles for one run.
///
/// `Clone` shares the redactor, resolved config and cancellation token rather
/// than copying them. It is not `Debug` because the interior of the redactor
/// registry is private and not meant to be dumped.
#[derive(Clone)]
pub struct RunContext {
    pub resolved: Arc<Resolved>,
    pub user: Option<String>,
    pub retain_body: bool,
    pub timeout: Duration,
    pub redactor: Arc<Redactor>,
    pub cancel: CancellationToken,
    pub events: Option<EventSink>,
    /// The run's file cache: one read per file, shared by every clone of this
    /// context, so a file referenced by five requests is read once (§7.14).
    /// A `Mutex` because `prepare` only ever has `&RunContext`.
    pub files: Arc<Mutex<FileCache>>,
}

impl RunContext {
    /// A context with sane defaults: `retain_body: true`, the timeout from
    /// [`Resolved`], a fresh cancellation token, and no event sink.
    #[must_use]
    pub fn new(resolved: Arc<Resolved>, redactor: Arc<Redactor>) -> Self {
        let files = FileCache::new(&resolved.root_dir);
        Self {
            timeout: resolved.timeout,
            resolved,
            user: None,
            retain_body: true,
            redactor,
            cancel: CancellationToken::new(),
            events: None,
            files: Arc::new(Mutex::new(files)),
        }
    }

    /// Set (or clear) the user this run operates as.
    #[must_use]
    pub fn with_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    /// Enable or disable body retention (§7.9).
    #[must_use]
    pub fn with_retain_body(mut self, retain_body: bool) -> Self {
        self.retain_body = retain_body;
        self
    }

    /// Attach (or detach) an event sink.
    #[must_use]
    pub fn with_events(mut self, events: Option<EventSink>) -> Self {
        self.events = events;
        self
    }

    /// Emit an event to the configured sink. A closed (dropped) channel is
    /// silently ignored — a disconnected frontend must never fail a run.
    ///
    /// The payload is scrubbed through this context's redactor first (§7.12):
    /// a `Failed` diagnostic and its hint are free text built from user-authored
    /// templates, so they can carry a secret the outcome-level redaction never
    /// sees. [`Event::redact`] scrubs every value-carrying field, including a
    /// `Finished` outcome's body; the other variants carry only counts and
    /// timings, so there is no raw byte payload to scrub.
    pub fn emit(&self, mut event: Event) {
        event.redact(&self.redactor);
        if let Some(sink) = &self.events {
            let _ = sink.send(event);
        }
    }
}
