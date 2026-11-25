//! Multipart assembly: the GraphQL multipart request spec and REST forms.
//!
//! A GraphQL upload is one POST whose body is three logical parts (§7.14,
//! after the jaydenseric spec): `operations` carries the JSON body with each
//! file variable nulled, `map` names the paths those files belong to, and
//! `0`, `1`, … carry the bytes. A REST `kind: multipart` body is the ordinary
//! form map plus file parts keyed by field name.
//!
//! Every builder here returns a `reqwest::multipart::Form`, which means the
//! boundary is reqwest's to choose and set — never ours (§7.6).

use std::collections::BTreeMap;

use bytes::Bytes;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::multipart::{Form, Part};
use serde_json::Value;

/// One file to send: `(field path, bytes, mime, filename)`.
///
/// The first element is the field name for a REST form or the full
/// `variables.<name>` path for a GraphQL `map` entry, so the `map` is built
/// from what the caller already knows rather than re-derived here.
pub type FileTuple = (String, Bytes, String, String);

/// One file part, already loaded, inside a prepared request.
#[derive(Debug, Clone)]
pub(crate) struct FilePart {
    /// Field name (REST) or `variables.<name>` (GraphQL).
    pub path: String,
    pub bytes: Bytes,
    pub mime: String,
    pub filename: String,
}

impl FilePart {
    /// The tuple form the public builders take.
    pub fn as_tuple(&self) -> FileTuple {
        (
            self.path.clone(),
            self.bytes.clone(),
            self.mime.clone(),
            self.filename.clone(),
        )
    }
}

/// The multipart body a [`crate::PreparedRequest`] will assemble on the wire.
///
/// `reqwest::multipart::Form` is neither `Clone` nor `Debug` and owns its
/// boundary, so the prepared request carries the plan and the wire builder
/// makes the form — once per attempt, boundary included.
#[derive(Debug, Clone)]
pub(crate) enum MultipartPlan {
    /// A GraphQL multipart request: `operations`, `map`, then the files.
    Graphql { body: Value, files: Vec<FilePart> },
    /// A REST multipart request: form fields plus files, keyed by field name.
    Rest {
        fields: BTreeMap<String, String>,
        files: Vec<FilePart>,
    },
}

/// Build the GraphQL multipart request described by `body` and `files`.
///
/// `body` is the resolved GraphQL body (`query`, `variables`, maybe
/// `operationName`). Every file's variable is set to `null` in `operations`,
/// `map` lists each file's `variables.<name>` path under its part index, and
/// the file parts are numbered from zero in the order given.
pub fn build_graphql_multipart(body: &Value, files: &[FileTuple]) -> Form {
    let mut operations = body.clone();
    if let Some(variables) = operations
        .get_mut("variables")
        .and_then(Value::as_object_mut)
    {
        for (path, ..) in files {
            let name = path.strip_prefix("variables.").unwrap_or(path);
            if let Some(slot) = variables.get_mut(name) {
                *slot = Value::Null;
            }
        }
    }

    let mut map = serde_json::Map::new();
    for (index, (path, ..)) in files.iter().enumerate() {
        map.insert(
            index.to_string(),
            Value::Array(vec![Value::String(path.clone())]),
        );
    }

    let mut form = Form::new()
        .part("operations", json_part(&operations))
        .part("map", json_part(&Value::Object(map)));
    for (index, (_, bytes, mime, filename)) in files.iter().enumerate() {
        form = form.part(index.to_string(), file_part(bytes, mime, filename));
    }
    form
}

/// Build a REST multipart form: one part per file, keyed by field name.
///
/// Text fields from `kind: multipart`'s flat map are added by the caller
/// (`prepare`), because they may sit beside the files on one request.
pub fn build_rest_multipart(files: &[FileTuple]) -> Form {
    let mut form = Form::new();
    for (field, bytes, mime, filename) in files {
        form = form.part(field.clone(), file_part(bytes, mime, filename));
    }
    form
}

/// A JSON part: the spec requires `Content-Type: application/json` on
/// `operations` and `map`, and a header map is infallible where `mime_str`
/// is not.
fn json_part(value: &Value) -> Part {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Part::text(value.to_string()).headers(headers)
}

/// A file part with its MIME type and filename, both already sanitised.
fn file_part(bytes: &Bytes, mime: &str, filename: &str) -> Part {
    let mut headers = HeaderMap::new();
    let value = HeaderValue::from_str(mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    headers.insert(CONTENT_TYPE, value);
    Part::bytes(bytes.to_vec())
        .file_name(filename.to_string())
        .headers(headers)
}
