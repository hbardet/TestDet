//! The `Event` boundary type and its sink (§7.10).
//!
//! `Event` is the live progress/result stream the engine pushes into an
//! [`EventSink`] while an operation runs. A TUI renders events into widgets, a
//! CLI renders them as lines (or suppresses them under `--format json`), and a
//! future web UI streams them. The sink is an `UnboundedSender` so the engine
//! never blocks on a slow or disconnected frontend.

use std::time::Duration;

use crate::diagnostic::Diagnostic;
use crate::outcome::Outcome;
use crate::redact::Redactor;

/// One progress or result event from a running operation (§7.10).
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        operation: String,
        user: Option<String>,
    },
    Sent {
        bytes: u64,
    },
    Received {
        status: u16,
        bytes: u64,
        elapsed: Duration,
    },
    Failed {
        diagnostic: Diagnostic,
    },
    Finished {
        outcome: Outcome,
    },
}

/// The channel the engine pushes [`Event`]s into. Unbounded, so a producer
/// never awaits a consumer.
pub type EventSink = tokio::sync::mpsc::UnboundedSender<Event>;

impl Event {
    /// Scrub every registered secret from this event's payload.
    ///
    /// A `Finished` event delegates to [`Outcome::redact`]; a `Failed` event
    /// scrubs the diagnostic's value-carrying fields (`message`, `hint`). The
    /// other variants carry only counts and timings and need no redaction.
    pub fn redact(&mut self, redactor: &Redactor) {
        match self {
            Event::Failed { diagnostic } => {
                diagnostic.message = redactor.scrub(&diagnostic.message);
                if let Some(hint) = &mut diagnostic.hint {
                    *hint = redactor.scrub(hint);
                }
            }
            Event::Finished { outcome } => outcome.redact(redactor),
            _ => {}
        }
    }
}
