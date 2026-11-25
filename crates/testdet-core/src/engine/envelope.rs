//! The GraphQL envelope: request body construction and defensive decoding.
//!
//! A GraphQL response is a JSON object whose `errors` entry is an array, but
//! a real server sits behind proxies, returns HTML error pages, PNGs from a
//! misconfigured gateway, or `null`. Nothing here may panic or return `Err`:
//! an unrecognised shape is a [`EnvelopeShape`] marker the frontend can render,
//! while the raw bytes stay in [`crate::Outcome::body`] for a human to read
//! (§7.9).

use serde_json::Value;

use crate::outcome::GraphQLError;

/// How a response body's GraphQL envelope looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeShape {
    /// A JSON object; `errors`, when present, was an array.
    Ok,
    /// Not UTF-8, or not JSON at all (an HTML error page, a PNG, …).
    NotJson,
    /// Valid JSON, but the top level is not an object (`null`, `[]`, `"x"`).
    NotAnObject,
    /// A JSON object whose `errors` entry exists but is not an array.
    ErrorsNotAnArray,
}

/// Build the `{"query", "variables", "operationName"}` body of a GraphQL
/// request.
///
/// `query` is embedded as-is (it is the operation's literal document, already
/// templated by the caller). `variables` is included only when it is a
/// non-empty object; `operationName` only when one is named.
pub fn graphql_body(query: &str, variables: &Value, operation_name: Option<&str>) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("query".to_string(), Value::String(query.to_string()));

    if let Some(variables) = variables.as_object()
        && !variables.is_empty()
    {
        body.insert("variables".to_string(), Value::Object(variables.clone()));
    }

    if let Some(name) = operation_name {
        body.insert("operationName".to_string(), Value::String(name.to_string()));
    }

    Value::Object(body)
}

/// Decode a response body's GraphQL envelope without ever failing.
///
/// The returned shape explains a missing error list: an HTML page is
/// `NotJson`, `null` is `NotAnObject`, `{"errors": "boom"}` is
/// `ErrorsNotAnArray`. An absent `errors` key is `Ok` with no errors — that is
/// what a successful GraphQL response looks like. Malformed individual error
/// entries degrade field by field (a missing `message` becomes empty, a
/// non-array `path` becomes `None`).
pub fn read_envelope(bytes: &[u8]) -> (Vec<GraphQLError>, EnvelopeShape) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return (Vec::new(), EnvelopeShape::NotJson);
    };

    let Value::Object(object) = value else {
        return (Vec::new(), EnvelopeShape::NotAnObject);
    };

    let Some(errors) = object.get("errors") else {
        return (Vec::new(), EnvelopeShape::Ok);
    };

    let Value::Array(entries) = errors else {
        return (Vec::new(), EnvelopeShape::ErrorsNotAnArray);
    };

    let errors = entries.iter().map(graphql_error).collect();
    (errors, EnvelopeShape::Ok)
}

/// One `errors[]` entry, best effort.
fn graphql_error(value: &Value) -> GraphQLError {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let path = value
        .get("path")
        .and_then(Value::as_array)
        .map(|segments| segments.iter().map(path_segment).collect());

    let extensions = value
        .get("extensions")
        .filter(|extensions| extensions.is_object())
        .cloned();

    GraphQLError {
        message,
        path,
        extensions,
    }
}

/// A `path` element: strings verbatim, numbers/others rendered as JSON text.
fn path_segment(value: &Value) -> String {
    match value {
        Value::String(segment) => segment.clone(),
        other => other.to_string(),
    }
}
