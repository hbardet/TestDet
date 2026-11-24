//! Typed conversion of validated config documents.
//!
//! Each entry point runs the schema pass ([`crate::config::validate::validate_document`])
//! first, then a hand-written per-file rules pass that reclassifies the schema's
//! collapsed tagged-union errors into their specific codes (`E_FOREIGN_FIELD`,
//! `E_FLAT_BODY_ONLY`, `E_FORBIDDEN_HEADER`) and adds the checks a JSON Schema
//! cannot express. If the combined set has any error, the caller gets the whole
//! [`Diagnostics`] (warnings travel with it) and no typed value is produced.
//! Only a clean document reaches [`serde_path_to_error`], whose residual error —
//! should one survive the schema pass — is reported once as `E_WRONG_TYPE` with
//! the dotted path from `err.path()` and the inner serde message.

use std::collections::BTreeSet;

use schemars::Schema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::Document;
use crate::config::model::{Manifest, UsersFile};
use crate::config::schema;
use crate::config::validate::{SchemaKind, validate_document};
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::operation::model::Operation;

/// Parse and validate a manifest file (`testdet.yaml`).
pub fn parse_manifest(doc: &Document) -> Result<Manifest, Diagnostics> {
    deserialize_validated(doc, &schema::manifest(), SchemaKind::Manifest)
}

/// Parse and validate an operation file.
pub fn parse_operation(doc: &Document) -> Result<Operation, Diagnostics> {
    deserialize_validated(doc, &schema::operation(), SchemaKind::Operation)
}

/// Parse and validate a users file (`testdet/users.yaml`).
pub fn parse_users(doc: &Document) -> Result<UsersFile, Diagnostics> {
    deserialize_validated(doc, &schema::users(), SchemaKind::Users)
}

/// The shared conversion pipeline for all three file kinds.
fn deserialize_validated<T>(
    doc: &Document,
    schema: &Schema,
    kind: SchemaKind,
) -> Result<T, Diagnostics>
where
    T: DeserializeOwned,
{
    let schema_diagnostics = validate_document(doc, schema, kind);
    let mut diagnostics = reclassify(doc, kind, schema_diagnostics);
    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

    let json = match serde_json::to_string(doc.root()) {
        Ok(json) => json,
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: Some(doc.path().to_path_buf()),
                span: None,
                path: None,
                message: format!("cannot serialise validated document: {error}"),
                hint: None,
            });
            return Err(diagnostics);
        }
    };

    match serde_path_to_error::deserialize(&mut serde_json::Deserializer::from_str(&json)) {
        Ok(value) => Ok(value),
        Err(error) => {
            let path = error.path().to_string();
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: Some(doc.path().to_path_buf()),
                span: doc.span_at(&path),
                path: Some(path),
                message: error.inner().to_string(),
                hint: None,
            });
            Err(diagnostics)
        }
    }
}

/// A rule the schema pass cannot (or only partially) express, plus its target path.
struct Rule {
    path: String,
    diagnostic: Diagnostic,
}

/// Reclassify the schema pass output for an operation file.
///
/// `kind` other than [`SchemaKind::Operation`] has no per-file rules yet, so its
/// schema diagnostics pass through unchanged.
fn reclassify(doc: &Document, kind: SchemaKind, diagnostics: Diagnostics) -> Diagnostics {
    if kind != SchemaKind::Operation {
        return diagnostics;
    }

    let rules = operation_rules(doc);
    if rules.is_empty() {
        return diagnostics;
    }

    let specific: BTreeSet<String> = rules.iter().map(|rule| rule.path.clone()).collect();
    let mut kept: Vec<Diagnostic> = diagnostics
        .iter()
        .filter(|diagnostic| !reclassified(diagnostic, &specific))
        .cloned()
        .collect();
    kept.extend(rules.into_iter().map(|rule| rule.diagnostic));
    sort_diagnostics(&mut kept);
    Diagnostics::from(kept)
}

/// Whether a schema diagnostic is superseded by a specific per-file rule.
fn reclassified(diagnostic: &Diagnostic, specific: &BTreeSet<String>) -> bool {
    // The schema collapses every tagged-union failure (`Request`/`Body`) into a
    // single `oneOf` `E_WRONG_TYPE` at `request`. Once a specific rule names the
    // real problem, that collapse is a duplicate.
    if diagnostic.code == codes::E_WRONG_TYPE && diagnostic.path.as_deref() == Some("request") {
        return true;
    }
    // Defensive: should the schema ever surface the granular error at the exact
    // path a rule now owns, drop it there too.
    diagnostic.path.as_deref().is_some_and(|path| {
        specific.contains(path)
            && matches!(
                diagnostic.code,
                codes::E_UNKNOWN_FIELD | codes::E_WRONG_TYPE
            )
    })
}

/// Collect the per-file operation rules: foreign request fields, non-flat form
/// bodies, and hand-set multipart `Content-Type` headers.
fn operation_rules(doc: &Document) -> Vec<Rule> {
    let mut rules = Vec::new();
    let file = Some(doc.path().to_path_buf());
    let root = doc.root();
    let Some(request) = root.get("request").and_then(Value::as_object) else {
        return rules;
    };

    if let Some(kind) = request.get("kind").and_then(Value::as_str) {
        let (foreign, other) = match kind {
            "graphql" => (&["method", "body"][..], "http"),
            "http" => (&["query", "operationName", "variables"][..], "graphql"),
            _ => (&[][..], ""),
        };
        for key in foreign {
            if request.contains_key(*key) {
                let path = format!("request.{key}");
                rules.push(Rule {
                    path: path.clone(),
                    diagnostic: Diagnostic {
                        code: codes::E_FOREIGN_FIELD,
                        severity: Severity::Error,
                        file: file.clone(),
                        span: doc.span_at(&path),
                        path: Some(path),
                        message: format!("`{key}` belongs to `kind: {other}`, not `kind: {kind}`"),
                        hint: Some("remove it, or switch `request.kind`".to_string()),
                    },
                });
            }
        }
    }

    if let Some(body) = request.get("body").and_then(Value::as_object) {
        let body_kind = body.get("kind").and_then(Value::as_str);

        if body_kind == Some("form")
            && let Some(value) = body.get("value").and_then(Value::as_object)
        {
            for (key, entry) in value {
                if is_scalar(entry) {
                    continue;
                }
                let path = format!("request.body.value.{key}");
                let shape = if entry.is_array() {
                    "a sequence"
                } else {
                    "a mapping"
                };
                rules.push(Rule {
                    path: path.clone(),
                    diagnostic: Diagnostic {
                        code: codes::E_FLAT_BODY_ONLY,
                        severity: Severity::Error,
                        file: file.clone(),
                        span: doc.span_at(&path),
                        path: Some(path),
                        message: format!("form bodies are flat; `{key}` is {shape}"),
                        hint: Some(
                            "flatten it, or use `kind: json` / `kind: multipart`".to_string(),
                        ),
                    },
                });
            }
        }

        if body_kind == Some("multipart")
            && let Some(headers) = request.get("headers").and_then(Value::as_object)
        {
            for key in headers.keys() {
                if !key.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                let path = format!("request.headers.{key}");
                rules.push(Rule {
                    path: path.clone(),
                    diagnostic: Diagnostic {
                        code: codes::E_FORBIDDEN_HEADER,
                        severity: Severity::Error,
                        file: file.clone(),
                        span: doc.span_at(&path),
                        path: Some(path),
                        message: format!("`{key}` must not be set on a multipart request"),
                        hint: Some(
                            "TestDet sets the multipart boundary; remove this header (set it on \
                             the environment if a gateway needs one)"
                                .to_string(),
                        ),
                    },
                });
            }
        }
    }

    rules
}

/// True for a scalar form value (string, number, boolean or null).
fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

/// Sort diagnostics by `(file, line, column)` with absent locations first, so
/// the reclassified output is stable regardless of rule order.
fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| {
                a.span
                    .map(|span| span.line)
                    .cmp(&b.span.map(|span| span.line))
            })
            .then_with(|| {
                a.span
                    .map(|span| span.column)
                    .cmp(&b.span.map(|span| span.column))
            })
    });
}
