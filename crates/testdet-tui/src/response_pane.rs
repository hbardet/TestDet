//! The response pane: status line, GraphQL errors, tabs and the body.
//!
//! Layout rules, top to bottom (§7.16):
//!
//! 1. the status line — `200 OK · 132 ms · 1.2 kB`;
//! 2. the tab bar — Body / Headers / Cookies;
//! 3. a transport-failure block, when the request never completed;
//! 4. the GraphQL error block — pinned ABOVE the body, even on a `200`;
//! 5. a warning when the response was not a GraphQL envelope (F5);
//! 6. the selected tab's content.
//!
//! Errors live above the body on purpose: the whole reason to open a GraphQL
//! response is usually that it failed while returning `200`.
//!
//! Timing and size come from [`testdet_core::Outcome`] and nowhere else. The
//! header promises only what the engine measures for every run — total time
//! and body size — because `connect` and `ttfb` are permanently `None` on the
//! HTTP path (§7.13).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use testdet_core::EnvelopeShape;
use testdet_core::outcome::{CookieInfo, GraphQLError, Outcome, Timing};

use crate::App;

/// Which tab of the response pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Body,
    Headers,
    Cookies,
}

impl Tab {
    /// The tab after this one, wrapping around.
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Body => Self::Headers,
            Self::Headers => Self::Cookies,
            Self::Cookies => Self::Body,
        }
    }

    /// The tab before this one, wrapping around.
    pub(crate) fn previous(self) -> Self {
        match self {
            Self::Body => Self::Cookies,
            Self::Headers => Self::Body,
            Self::Cookies => Self::Headers,
        }
    }

    /// The tab bar as one line, the active tab in brackets.
    fn bar(self) -> String {
        [Self::Body, Self::Headers, Self::Cookies]
            .into_iter()
            .map(|tab| {
                let name = match tab {
                    Self::Body => "Body",
                    Self::Headers => "Headers",
                    Self::Cookies => "Cookies",
                };
                if tab == self {
                    format!("[{name}]")
                } else {
                    name.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// Draw the pane.
pub(crate) fn draw(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let lines = match app.outcome() {
        Some(outcome) => outcome_lines(app, outcome),
        None => vec![Line::from(match app.running_operation() {
            Some(operation) => format!("waiting for {operation}…"),
            None => "no response yet".to_string(),
        })],
    };
    let block = Block::bordered().title("response");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Everything the pane draws for a finished outcome.
fn outcome_lines(app: &App, outcome: &Outcome) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(status_line(
            outcome.status,
            &outcome.timing,
            outcome.body_size,
        )),
        Line::from(app.tab().bar()),
        Line::from(""),
    ];
    if let Some(error) = &outcome.transport_error {
        lines.push(Line::from(format!(
            "transport error ({}): {}",
            transport_label(&error.kind),
            error.message
        )));
        lines.push(Line::from(""));
    }
    if !outcome.graphql_errors.is_empty() {
        lines.push(Line::from(format!(
            "graphql errors ({})",
            outcome.graphql_errors.len()
        )));
        for error in &outcome.graphql_errors {
            lines.extend(graphql_error_lines(error));
        }
        lines.push(Line::from(""));
    }
    if outcome.shape != EnvelopeShape::Ok {
        lines.push(Line::from(format!(
            "warning: the server did not return a GraphQL envelope ({})",
            envelope_label(outcome.shape)
        )));
        lines.push(Line::from(""));
    }
    match app.tab() {
        Tab::Body => lines.extend(body_lines(app)),
        Tab::Headers => lines.extend(header_lines(app, outcome)),
        Tab::Cookies => lines.extend(cookie_lines(app, &outcome.cookies)),
    }
    lines
}

/// The status line: `200 OK · 132 ms · 1.2 kB`, or the failure's shape.
fn status_line(status: u16, timing: &Timing, body_size: u64) -> String {
    format!(
        "{} · {} ms · {}",
        status_label(status),
        timing.total.as_millis(),
        human_size(body_size)
    )
}

/// `200 OK` for the codes an API actually returns; the bare number otherwise.
fn status_label(status: u16) -> String {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    };
    if status == 0 {
        "no response".to_string()
    } else if reason.is_empty() {
        status.to_string()
    } else {
        format!("{status} {reason}")
    }
}

/// Body size for humans: bytes below 1 kB, one decimal above.
fn human_size(size: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let size = size as f64;
    if size < 1000.0 {
        format!("{size:.0} B")
    } else if size < 1_000_000.0 {
        format!("{:.1} kB", size / 1000.0)
    } else {
        format!("{:.1} MB", size / 1_000_000.0)
    }
}

/// One GraphQL error, indented under the block header, with its path.
fn graphql_error_lines(error: &GraphQLError) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!("  ✗ {}", error.message))];
    if let Some(path) = &error.path
        && !path.is_empty()
    {
        lines.push(Line::from(format!("    at {}", path.join("."))));
    }
    lines
}

/// The body tab: the summary line when collapsed, otherwise the lines.
fn body_lines(app: &App) -> Vec<Line<'static>> {
    match app.body() {
        Some(body) => body.visible_lines(),
        None => vec![Line::from("(no body)")],
    }
}

/// The headers tab. Header values always go through the redactor, and the
/// fallback rendering is the name plus `-CENSORED`, never the raw value.
fn header_lines(app: &App, outcome: &Outcome) -> Vec<Line<'static>> {
    if outcome.headers.is_empty() {
        return vec![Line::from("(no headers)")];
    }
    outcome
        .headers
        .iter()
        .map(|(name, value)| Line::from(format!("{name}: {}", app.scrub(value))))
        .collect()
}

/// The cookies tab: name, censored value, flags, domain and path.
fn cookie_lines(app: &App, cookies: &[CookieInfo]) -> Vec<Line<'static>> {
    if cookies.is_empty() {
        return vec![Line::from("(no cookies)")];
    }
    cookies
        .iter()
        .map(|cookie| {
            let mut flags = Vec::new();
            if cookie.http_only {
                flags.push("HttpOnly");
            }
            if cookie.secure {
                flags.push("Secure");
            }
            let flags = if flags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", flags.join(", "))
            };
            Line::from(format!(
                "{} = {}{flags} domain={} path={}",
                cookie.name,
                app.scrub(&cookie.value),
                cookie.domain,
                cookie.path
            ))
        })
        .collect()
}

/// The engine's transport-failure kinds, spelled out for the pane.
fn transport_label(kind: &testdet_core::outcome::TransportKind) -> &'static str {
    use testdet_core::outcome::TransportKind;
    match kind {
        TransportKind::Connect => "connect",
        TransportKind::Timeout => "timeout",
        TransportKind::Read => "read",
        TransportKind::Tls => "tls",
        TransportKind::Cancelled => "cancelled",
        TransportKind::Prepare => "prepare",
        TransportKind::Other => "other",
    }
}

/// Why a body is not an envelope (F5), in words rather than enum names.
fn envelope_label(shape: EnvelopeShape) -> &'static str {
    match shape {
        EnvelopeShape::Ok => "ok",
        EnvelopeShape::NotJson => "not json",
        EnvelopeShape::NotAnObject => "not an object",
        EnvelopeShape::ErrorsNotAnArray => "errors is not an array",
    }
}
