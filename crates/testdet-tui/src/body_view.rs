//! The response body, turned into display lines.
//!
//! Three shapes come out of here, and the rules are deliberately blunt:
//!
//! * valid JSON (per the body's declared `content-type`, or because it plainly
//!   looks like JSON) is pretty-printed;
//! * anything else that is valid UTF-8 is shown as-is, line for line;
//! * bytes that are not UTF-8 become `binary (N bytes)`.
//!
//! The last rule is the important one: a viewer that silently replaces invalid
//! bytes with `U+FFFD` lies about what the server sent (§7.16). Nothing here
//! can panic — the input is bytes from the network.

use ratatui::text::Line;
use testdet_core::serde_json;

/// A response body prepared for the response pane.
///
/// The struct is public and its fields are public on purpose: the TUI is the
/// product, and a host that drives [`crate::App`] may want to inspect what the
/// pane is about to draw.
#[derive(Debug, Clone)]
pub struct BodyView {
    /// One display line per body line; never empty (a placeholder takes over).
    pub lines: Vec<Line<'static>>,
    /// Collapsed bodies draw [`BodyView::summary`] instead of `lines`.
    pub collapsed: bool,
    /// Case-insensitive substring filter, applied to the display lines.
    pub filter: Option<String>,
}

impl BodyView {
    /// Build a view from raw body bytes.
    ///
    /// `content_type` is the response's `content-type` header when there was
    /// one; it only ever *promotes* a body to JSON. A body that is valid UTF-8
    /// but not valid JSON is shown verbatim rather than reported as an error.
    pub fn from_bytes(bytes: &[u8], content_type: Option<&str>) -> Self {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return Self::placeholder(format!("binary ({} bytes)", bytes.len()));
        };
        if text.is_empty() {
            return Self::placeholder("(empty body)");
        }
        let declared_json = content_type.is_some_and(|value| value.contains("json"));
        let looks_json = text.trim_start().starts_with(['{', '[']);
        let rendered = if declared_json || looks_json {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
        } else {
            None
        };
        let lines: Vec<Line<'static>> = rendered
            .unwrap_or_else(|| text.to_string())
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
        if lines.is_empty() {
            return Self::placeholder("(empty body)");
        }
        Self {
            lines,
            collapsed: false,
            filter: None,
        }
    }

    /// A body the engine did not retain (`--no-body`, or a refused read).
    ///
    /// Only the size is known, so only the size is shown — the same shape as
    /// the non-UTF-8 case, which is also just a size.
    pub fn not_retained(body_size: u64) -> Self {
        Self::placeholder(format!("body not retained ({body_size} bytes)"))
    }

    /// The single line a collapsed body draws.
    pub fn summary(&self) -> String {
        format!(
            "body: {} line{} (collapsed — press c to expand)",
            self.lines.len(),
            if self.lines.len() == 1 { "" } else { "s" }
        )
    }

    /// The lines to draw: filtered when a filter is set, the summary when
    /// collapsed.
    pub fn visible_lines(&self) -> Vec<Line<'static>> {
        if self.collapsed {
            return vec![Line::from(self.summary())];
        }
        let Some(filter) = self.filter.as_deref().map(str::trim) else {
            return self.lines.clone();
        };
        if filter.is_empty() {
            return self.lines.clone();
        }
        let needle = filter.to_lowercase();
        let matching: Vec<Line<'static>> = self
            .lines
            .iter()
            .filter(|line| line.to_string().to_lowercase().contains(&needle))
            .cloned()
            .collect();
        if matching.is_empty() {
            return vec![Line::from(format!("(no body lines match `{filter}`)"))];
        }
        matching
    }

    /// Toggle the collapsed flag, keeping `lines` and `filter` intact.
    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
    }

    /// Set (or clear, with `None`) the body filter.
    pub fn set_filter(&mut self, filter: Option<&str>) {
        self.filter = filter
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    fn placeholder(text: impl Into<String>) -> Self {
        Self {
            lines: vec![Line::from(text.into())],
            collapsed: false,
            filter: None,
        }
    }
}
