//! Diagnostic model, code registry and renderers.
//!
//! `Diagnostic` is the vocabulary the whole project speaks: every later
//! subsystem (config loading, schema validation, typed conversion, templating,
//! HTTP, CLI commands) produces `Diagnostic`s, and the CLI renders them.
//!
//! This module owns both renderers because the JSON shape is a contract, not
//! an accident of whatever error-reporting crate happens to be in play. Only
//! `testdet-cli` performs terminal I/O; these functions return `String`.

use std::path::PathBuf;

/// Severity of a diagnostic. `Error` is worse than `Warning` and drives the
/// process exit code (1 vs 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    /// Lowercase name used in the JSON representation.
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }

    /// Rank so that `Error > Warning` regardless of declaration order.
    fn rank(self) -> u8 {
        match self {
            Severity::Error => 2,
            Severity::Warning => 1,
        }
    }
}

impl PartialOrd for Severity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Severity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// A 1-based position within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub line: u32,
    pub column: u32,
}

/// A single diagnostic. `file` and `span` together locate the problem in a
/// source file; `path` is the logical location inside a document (e.g. a YAML
/// path); `message` describes the problem; `hint` suggests a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub file: Option<PathBuf>,
    pub span: Option<Span>,
    pub path: Option<String>,
    pub message: String,
    pub hint: Option<String>,
}

/// An ordered collection of diagnostics. Order is insertion order and is
/// preserved by `render_text`, so deterministic insertion yields deterministic
/// output (Task 12 snapshots depend on this).
#[derive(Debug, Default, Clone)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn extend<I: IntoIterator<Item = Diagnostic>>(&mut self, iter: I) {
        self.items.extend(iter);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// True when at least one diagnostic is an error (exit code 1).
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    /// The most severe diagnostic present, or `None` when empty.
    pub fn worst_severity(&self) -> Option<Severity> {
        self.items.iter().map(|d| d.severity).max()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Diagnostic> {
        self.items.iter()
    }
}

impl From<Vec<Diagnostic>> for Diagnostics {
    fn from(items: Vec<Diagnostic>) -> Self {
        Self { items }
    }
}

/// Render diagnostics as human-readable text. One block per diagnostic, in
/// insertion order, separated by a blank line:
///
/// ```text
/// file:line:column
///   path
/// CODE  message
/// hint: text
/// ```
///
/// A diagnostic with no `file` omits the location line; a `file` without a
/// `span` prints the file path alone; one with no `path` omits the path line;
/// one with no `hint` omits the hint line.
pub fn render_text(diagnostics: &Diagnostics) -> String {
    let mut out = String::new();
    for (i, diagnostic) in diagnostics.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }

        match (&diagnostic.file, diagnostic.span) {
            (Some(file), Some(span)) => {
                out.push_str(&format!("{}:{}:{}", file.display(), span.line, span.column));
                out.push('\n');
            }
            (Some(file), None) => {
                out.push_str(&file.display().to_string());
                out.push('\n');
            }
            (None, _) => {}
        }

        if let Some(path) = &diagnostic.path {
            out.push_str("  ");
            out.push_str(path);
            out.push('\n');
        }

        out.push_str(diagnostic.code);
        out.push_str("  ");
        out.push_str(&diagnostic.message);
        out.push('\n');

        if let Some(hint) = &diagnostic.hint {
            out.push_str("hint: ");
            out.push_str(hint);
            out.push('\n');
        }
    }
    out
}

/// Render diagnostics as a JSON array. Key order follows the struct field
/// order below (`code`, `severity`, `file`, `line`, `column`, `path`,
/// `message`, `hint`); absent optional fields are omitted rather than `null`.
pub fn render_json(diagnostics: &Diagnostics) -> String {
    #[derive(serde::Serialize)]
    struct JsonDiagnostic<'a> {
        code: &'a str,
        severity: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<std::borrow::Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<&'a str>,
        message: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'a str>,
    }

    let items: Vec<JsonDiagnostic<'_>> = diagnostics
        .iter()
        .map(|d| JsonDiagnostic {
            code: d.code,
            severity: d.severity.as_str(),
            file: d.file.as_ref().map(|f| f.to_string_lossy()),
            line: d.span.map(|s| s.line),
            column: d.span.map(|s| s.column),
            path: d.path.as_deref(),
            message: &d.message,
            hint: d.hint.as_deref(),
        })
        .collect();

    serde_json::to_string(&items).expect("serializing diagnostics cannot fail")
}

/// Diagnostic codes. Each constant's value is its own name so that a rendered
/// diagnostic is self-describing and greppable.
pub mod codes {
    pub const E_UNKNOWN_FIELD: &str = "E_UNKNOWN_FIELD";
    pub const E_WRONG_TYPE: &str = "E_WRONG_TYPE";
    pub const E_MISSING_FIELD: &str = "E_MISSING_FIELD";
    pub const E_UNKNOWN_VAR: &str = "E_UNKNOWN_VAR";
    pub const E_UNKNOWN_FILE: &str = "E_UNKNOWN_FILE";
    pub const E_UNKNOWN_ENV: &str = "E_UNKNOWN_ENV";
    pub const E_UNKNOWN_USER: &str = "E_UNKNOWN_USER";
    pub const E_FOREIGN_FIELD: &str = "E_FOREIGN_FIELD";
    pub const E_DUPLICATE_ID: &str = "E_DUPLICATE_ID";
    pub const E_DUPLICATE_USER: &str = "E_DUPLICATE_USER";
    pub const E_FLAT_BODY_ONLY: &str = "E_FLAT_BODY_ONLY";
    pub const E_FORBIDDEN_HEADER: &str = "E_FORBIDDEN_HEADER";
    pub const E_PATH_ESCAPE: &str = "E_PATH_ESCAPE";
    /// A file exceeded the 5 MB upload ceiling. The spec (§7.14) describes the
    /// ceiling but names no code for it; recorded as a gap.
    pub const E_FILE_TOO_LARGE: &str = "E_FILE_TOO_LARGE";
    pub const E_UNRESOLVED_REF: &str = "E_UNRESOLVED_REF";
    pub const E_MULTI_OPERATION: &str = "E_MULTI_OPERATION";
    pub const E_VERSION: &str = "E_VERSION";
    pub const E_DUPLICATE_KEY: &str = "E_DUPLICATE_KEY";
    pub const E_TRANSPORT: &str = "E_TRANSPORT";
    /// A `capture:` path did not resolve in the JSON response body. The spec
    /// (§7.7, §7.13) has no dedicated code for this; recorded as a gap.
    pub const E_UNRESOLVED_CAPTURE: &str = "E_UNRESOLVED_CAPTURE";
    pub const W_UNUSED_VAR: &str = "W_UNUSED_VAR";
    pub const W_UNSET_SECRET: &str = "W_UNSET_SECRET";
    pub const W_MULTI_OPERATION: &str = "W_MULTI_OPERATION";
    pub const W_ID_FILENAME_MISMATCH: &str = "W_ID_FILENAME_MISMATCH";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_omits_absent_optionals_and_orders_keys() {
        let d = Diagnostic {
            code: codes::E_UNKNOWN_VAR,
            severity: Severity::Error,
            file: Some("testdet/collection/getUser.yaml".into()),
            span: Some(Span {
                line: 17,
                column: 24,
            }),
            path: Some("headers.Authorization".into()),
            message: "'{{env.API_TOKN}}' does not resolve".into(),
            hint: Some("did you mean API_TOKEN?".into()),
        };
        let json = render_json(&Diagnostics::from(vec![d]));
        assert_eq!(
            json,
            r#"[{"code":"E_UNKNOWN_VAR","severity":"error","file":"testdet/collection/getUser.yaml","line":17,"column":24,"path":"headers.Authorization","message":"'{{env.API_TOKN}}' does not resolve","hint":"did you mean API_TOKEN?"}]"#
        );
    }

    #[test]
    fn json_omits_all_absent_optionals() {
        let d = Diagnostic {
            code: codes::E_VERSION,
            severity: Severity::Warning,
            file: None,
            span: None,
            path: None,
            message: "unsupported version".into(),
            hint: None,
        };
        let json = render_json(&Diagnostics::from(vec![d]));
        assert_eq!(
            json,
            r#"[{"code":"E_VERSION","severity":"warning","message":"unsupported version"}]"#
        );
    }

    #[test]
    fn text_omits_location_and_path_when_absent() {
        let d = Diagnostic {
            code: codes::W_UNSET_SECRET,
            severity: Severity::Warning,
            file: None,
            span: None,
            path: None,
            message: "secret not set".into(),
            hint: None,
        };
        let text = render_text(&Diagnostics::from(vec![d]));
        assert_eq!(text, "W_UNSET_SECRET  secret not set\n");
    }

    #[test]
    fn text_separates_blocks_with_blank_line() {
        let first = Diagnostic {
            code: codes::E_UNKNOWN_ENV,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: "first".into(),
            hint: None,
        };
        let second = Diagnostic {
            code: codes::W_UNUSED_VAR,
            severity: Severity::Warning,
            file: None,
            span: None,
            path: None,
            message: "second".into(),
            hint: None,
        };
        let text = render_text(&Diagnostics::from(vec![first, second]));
        assert_eq!(text, "E_UNKNOWN_ENV  first\n\nW_UNUSED_VAR  second\n");
    }
}
