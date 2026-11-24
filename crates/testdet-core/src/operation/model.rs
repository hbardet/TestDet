//! Typed operation model: operation files and their requests (§7.6, §7.14).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::model::HeaderMap;

/// One operation file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Operation {
    /// `id`
    pub id: String,
    /// `description`
    pub description: Option<String>,
    /// `tags`
    #[serde(default)]
    pub tags: Vec<String>,
    /// `request`
    pub request: Request,
    /// `vars`, flat templating variables carrying typed JSON values (§7.8).
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
    /// `files`, referenced from variables as `{{file.<name>}}` (§7.14).
    #[serde(default)]
    pub files: BTreeMap<String, FileSpec>,
    /// `capture`, flat map of name → dot-path into the JSON response body.
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
}

/// A request, discriminated on `kind`.
///
/// Internally tagged: the `kind` field selects the variant, and a field that
/// belongs to the other variant (e.g. `method` on a `graphql` request) is an
/// unknown field, reported as `E_FOREIGN_FIELD` by Task 7.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Request {
    /// `kind: graphql`
    Graphql(GraphqlRequest),
    /// `kind: http`
    Http(HttpRequest),
}

/// A GraphQL request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GraphqlRequest {
    /// `path`, overrides the manifest `graphqlPath`.
    pub path: Option<String>,
    /// `query`
    pub query: String,
    /// `operationName`
    pub operation_name: Option<String>,
    /// `variables`, the GraphQL payload sent inside `{"query", "variables"}`.
    #[serde(default)]
    pub variables: BTreeMap<String, serde_json::Value>,
    /// `headers`
    #[serde(default)]
    pub headers: HeaderMap,
}

/// An HTTP (REST) request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpRequest {
    /// `method`
    pub method: String,
    /// `path`
    pub path: String,
    /// `body`
    pub body: Option<Body>,
    /// `headers`
    #[serde(default)]
    pub headers: HeaderMap,
}

/// A request body, discriminated on `kind` (§7.6).
///
/// Adjacent tagging with content key `value`, matching the spec example
/// `body: { kind: json, value: {...} }`. The same `value` key carries the
/// payload for every body kind; the spec only pins it for `json`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Body {
    /// `kind: json`, any JSON value.
    Json(serde_json::Value),
    /// `kind: text`
    Text(String),
    /// `kind: form`, flat map only (nested value is `E_FLAT_BODY_ONLY`).
    Form(BTreeMap<String, String>),
    /// `kind: multipart`, flat map only.
    Multipart(BTreeMap<String, String>),
    /// `kind: rawFile`, a path string.
    RawFile(String),
}

/// An entry in the operation's `files:` block (§7.14).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileSpec {
    /// `path`, relative to the config file's directory.
    pub path: String,
    /// `mimeType`, optional; sniffed from magic bytes when absent.
    pub mime_type: Option<String>,
    /// `filename`, optional `Content-Disposition` filename.
    pub filename: Option<String>,
    /// `encoding`, `multipart` (default) or `base64`.
    #[serde(default)]
    pub encoding: FileEncoding,
}

/// How a file's bytes reach the wire (§7.14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileEncoding {
    /// `multipart`
    #[default]
    Multipart,
    /// `base64`
    Base64,
}
