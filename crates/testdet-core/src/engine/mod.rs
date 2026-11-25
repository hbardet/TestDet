//! The engine: HTTP transport plus the defensive GraphQL envelope.
//!
//! [`execute`] is the one entry point a frontend calls to run an operation: it
//! resolves the operation's templates, merges its headers in precedence order,
//! sends the request, reads the response and returns an [`Outcome`] — never a
//! `Result` and never a panic, because a run of sixty operations must not stop
//! at the first bad backend (§7.3, §7.9).
//!
//! Events follow §7.10: `Started` before anything is resolved, `Sent` before
//! each network attempt, `Received` once the last byte has arrived,
//! `Failed` (code `E_TRANSPORT`) for a transport failure, and `Finished` last.
//! Cancellation is checked before sending, while awaiting the response, and
//! while reading the body.
//!
//! Timing: `total` is measured around the whole attempt; `connect` and `ttfb`
//! stay `None` because the high-level `reqwest` API does not expose connection
//! or first-byte timestamps (the fields exist in [`crate::Timing`] for a
//! client that can).

pub mod envelope;
pub mod multipart;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue, SET_COOKIE};
use serde_json::Value;

use crate::context::RunContext;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes, render_text};
use crate::events::Event;
use crate::files::LoadedFile;
use crate::operation::model::{Body, FileEncoding, FileSpec, Operation, Request};
use crate::outcome::{CookieInfo, Outcome, RequestEcho, Timing, TransportError, TransportKind};
use crate::secrets::build_source;
use crate::session::SessionStore;
use crate::template::value::{Segment, scan};
use crate::template::{Resolver, Token, parse_token};

use envelope::graphql_body;
pub use envelope::{EnvelopeShape, read_envelope};
use multipart::{FilePart, MultipartPlan, build_graphql_multipart, build_rest_multipart};

/// The pause between transport retries (§7.9 defaults: off unless configured).
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// The set of bytes left unencoded in a form body: RFC 3986 unreserved
/// characters. A space encodes as `%20`, which every form parser accepts.
const FORM_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A request ready to go on the wire, templates already resolved.
///
/// `headers` is the final merged list (response order on the wire is this
/// order), `body` the encoded bytes, and `content_type` the body's default
/// `Content-Type` — applied only when no merged header already sets one.
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub method: reqwest::Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
    pub content_type: Option<String>,
    /// A multipart body to assemble at wire time, where the boundary is
    /// `reqwest`'s to choose (§7.6). Never set together with `body`.
    pub(crate) multipart: Option<MultipartPlan>,
}

/// The body side of a prepared request: bytes, the default `Content-Type` for
/// those bytes, or the plan for a multipart body.
struct PreparedBody {
    body: Option<Bytes>,
    content_type: Option<String>,
    multipart: Option<MultipartPlan>,
}

impl PreparedBody {
    fn bytes(body: Option<Bytes>, content_type: Option<&str>) -> Self {
        Self {
            body,
            content_type: content_type.map(str::to_string),
            multipart: None,
        }
    }

    fn multipart(plan: MultipartPlan) -> Self {
        Self {
            body: None,
            content_type: None,
            multipart: Some(plan),
        }
    }
}

/// Resolve `operation`'s templates and build the request that will be sent.
///
/// Header precedence is lowest to highest: manifest defaults, environment,
/// user, then the operation's own headers ([`crate::config::Resolved::headers_for`]
/// supplies the first three layers and already folds the operation layer in).
/// The operation's headers are applied once more so precedence holds even if
/// that helper changes; names are matched case-insensitively and the last
/// value wins, so no header is ever sent twice.
pub fn prepare(ctx: &RunContext, operation: &Operation) -> Result<PreparedRequest, Diagnostics> {
    let session = BTreeMap::new();
    prepare_with_session(ctx, operation, &session, &[])
}

/// [`prepare`] with the calling user's captured session variables and
/// per-session default headers (§7.7). Session headers are inserted at the user
/// layer: the operation's own headers still win.
pub(crate) fn prepare_with_session(
    ctx: &RunContext,
    operation: &Operation,
    session: &BTreeMap<String, Value>,
    session_headers: &[(String, String)],
) -> Result<PreparedRequest, Diagnostics> {
    let resolved = &ctx.resolved;
    let vars = resolved.vars_for(operation, ctx.user.as_deref());
    // `Resolved::secrets.file` is already joined against the config root, so
    // an empty root leaves it exactly as resolution computed it.
    let env = build_source(&resolved.secrets, Path::new(""));
    let resolver = Resolver {
        vars: &vars,
        session,
        env: env.as_ref(),
        files: &operation.files,
    };

    let mut diagnostics = Diagnostics::new();

    let headers = prepare_headers(
        &resolver,
        resolved,
        operation,
        ctx.user.as_deref(),
        session_headers,
        &mut diagnostics,
    );

    let (method, url, prepared) = match &operation.request {
        Request::Graphql(request) => {
            prepare_graphql(ctx, &resolver, resolved, request, &mut diagnostics)
        }
        Request::Http(request) => prepare_http(ctx, &resolver, resolved, request, &mut diagnostics),
    };

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

    Ok(PreparedRequest {
        method,
        url,
        headers,
        body: prepared.body,
        content_type: prepared.content_type,
        multipart: prepared.multipart,
    })
}

/// Run one operation in the store's current user context, sending through
/// that user's session client.
///
/// This is the pre-session entry point: it does **not** log in. Use
/// [`crate::session::execute_for_user`] when the user may need a session
/// (login, capture, one re-login on auth failure).
pub async fn execute(store: &mut SessionStore, operation: &Operation) -> Outcome {
    let user = store.user().map(str::to_string);
    store.run(user.as_deref(), operation, false).await
}

/// Run one operation: send it, read the response, decode a GraphQL envelope,
/// redact, and return the outcome. The `Finished` event is always emitted,
/// whatever happened before it.
///
/// `client` comes from the caller's [`SessionStore`], so cookies and
/// connection pools stay with one identity (§7.7.5); `session` carries the
/// captured `{{session.*}}` values and `session_headers` the session's default
/// headers (a static token).
pub(crate) async fn execute_with_session(
    ctx: &RunContext,
    operation: &Operation,
    client: &reqwest::Client,
    session: &BTreeMap<String, Value>,
    session_headers: &[(String, String)],
) -> Outcome {
    ctx.emit(Event::Started {
        operation: operation.id.clone(),
        user: ctx.user.clone(),
    });

    let request = match prepare_with_session(ctx, operation, session, session_headers) {
        Ok(request) => request,
        Err(diagnostics) => return failure_outcome(ctx, operation, &diagnostics),
    };

    let mut outcome = send(ctx, request, client).await;
    outcome.operation = operation.id.clone();

    if matches!(&operation.request, Request::Graphql(_))
        && let Some(body) = &outcome.body
    {
        let (errors, _shape) = read_envelope(body);
        outcome.graphql_errors = errors;
    }

    outcome.redact(&ctx.redactor);
    ctx.emit(Event::Finished {
        outcome: outcome.clone(),
    });
    outcome
}

/// The header layers, resolved and merged: `headers_for` first, then the
/// operation's own headers, with last-wins case-insensitive replacement.
fn prepare_headers(
    resolver: &Resolver<'_>,
    resolved: &crate::config::Resolved,
    operation: &Operation,
    user: Option<&str>,
    session_headers: &[(String, String)],
    diagnostics: &mut Diagnostics,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();

    let mut session_layer = crate::config::model::HeaderMap::default();
    for (name, value) in session_headers {
        session_layer.0.insert(name.clone(), Some(value.clone()));
    }

    let layers = [
        resolved.headers_for(operation, user),
        session_layer,
        operation.request.headers().clone(),
    ];
    for layer in &layers {
        for (name, value) in &layer.0 {
            let Some(raw) = value else {
                continue;
            };
            let Some(value) = resolve_header_value(resolver, raw, diagnostics) else {
                continue;
            };
            if validate_header(name, &value, diagnostics) {
                upsert_header(&mut headers, name, value);
            }
        }
    }

    headers
}

/// Set `name` in place when already present (any casing), else append.
fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some(entry) = headers
        .iter_mut()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        entry.1 = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

/// Build a GraphQL request: POST, JSON envelope, default or per-request path.
///
/// File variables split two ways (§7.14): `encoding: multipart` becomes a file
/// part whose `operations` slot is nulled, while `encoding: base64` is
/// inlined as a JSON string and no multipart body is built.
fn prepare_graphql(
    ctx: &RunContext,
    resolver: &Resolver<'_>,
    resolved: &crate::config::Resolved,
    request: &crate::operation::model::GraphqlRequest,
    diagnostics: &mut Diagnostics,
) -> (reqwest::Method, Url, PreparedBody) {
    let path = request
        .path
        .as_deref()
        .map(|path| resolve_text(resolver, path, diagnostics).unwrap_or_else(|| path.to_string()))
        .unwrap_or_else(|| resolved.graphql_path.clone());
    let url = resolved.url_for(&path);

    let query = resolve_text(resolver, &request.query, diagnostics)
        .unwrap_or_else(|| request.query.clone());

    let object = Value::Object(request.variables.clone().into_iter().collect());
    let mut variables = resolve_value(resolver, &object, diagnostics);

    let mut files: Vec<FilePart> = Vec::new();
    for (name, value) in &request.variables {
        let Some(spec_name) = value.as_str().and_then(whole_file_ref) else {
            continue;
        };
        let Some(spec) = resolver.files.get(&spec_name) else {
            continue;
        };
        let Some(file) = load_file(ctx, spec, &format!("files.{spec_name}"), diagnostics) else {
            continue;
        };
        match spec.encoding {
            FileEncoding::Base64 => {
                if let Some(slots) = variables.as_object_mut() {
                    slots.insert(name.clone(), Value::String(STANDARD.encode(&file.bytes)));
                }
            }
            FileEncoding::Multipart => files.push(FilePart {
                path: format!("variables.{name}"),
                bytes: file.bytes,
                mime: file.mime,
                filename: file.filename,
            }),
        }
    }

    let body_value = graphql_body(&query, &variables, request.operation_name.as_deref());
    let prepared = if files.is_empty() {
        PreparedBody::bytes(
            encode_json(&body_value, diagnostics),
            Some("application/json"),
        )
    } else {
        PreparedBody::multipart(MultipartPlan::Graphql {
            body: body_value,
            files,
        })
    };

    (reqwest::Method::POST, url, prepared)
}

/// Build an HTTP request: method, path and body by kind.
fn prepare_http(
    ctx: &RunContext,
    resolver: &Resolver<'_>,
    resolved: &crate::config::Resolved,
    request: &crate::operation::model::HttpRequest,
    diagnostics: &mut Diagnostics,
) -> (reqwest::Method, Url, PreparedBody) {
    let method = parse_method(&request.method, diagnostics);

    let path =
        resolve_text(resolver, &request.path, diagnostics).unwrap_or_else(|| request.path.clone());
    let url = resolved.url_for(&path);

    let prepared = match &request.body {
        None => PreparedBody::bytes(None, None),
        Some(Body::Json(value)) => {
            let mut value = resolve_value(resolver, value, diagnostics);
            inline_base64_files(ctx, resolver, &mut value, "request.body.value", diagnostics);
            PreparedBody::bytes(encode_json(&value, diagnostics), Some("application/json"))
        }
        Some(Body::Text(text)) => {
            let text = resolve_text(resolver, text, diagnostics).unwrap_or_else(|| text.clone());
            PreparedBody::bytes(Some(Bytes::from(text)), Some("text/plain; charset=utf-8"))
        }
        Some(Body::Form(fields)) => PreparedBody::bytes(
            Some(Bytes::from(encode_form(fields, resolver, diagnostics))),
            Some("application/x-www-form-urlencoded"),
        ),
        Some(Body::Multipart(fields)) => {
            let plan = prepare_rest_multipart(ctx, resolver, fields, diagnostics);
            PreparedBody::multipart(plan)
        }
        Some(Body::RawFile(text)) => prepare_raw_file(ctx, resolver, text, diagnostics),
    };

    (method, url, prepared)
}

/// Assemble a REST `kind: multipart` body: text fields from the flat map plus
/// file parts keyed by field name, on one request (§7.14).
fn prepare_rest_multipart(
    ctx: &RunContext,
    resolver: &Resolver<'_>,
    fields: &BTreeMap<String, String>,
    diagnostics: &mut Diagnostics,
) -> MultipartPlan {
    let mut text: BTreeMap<String, String> = BTreeMap::new();
    let mut files: Vec<FilePart> = Vec::new();

    for (name, value) in fields {
        let Some(spec_name) = whole_file_ref(value) else {
            if let Some(resolved) = resolve_text(resolver, value, diagnostics) {
                text.insert(name.clone(), resolved);
            }
            continue;
        };
        let Some(spec) = resolver.files.get(&spec_name) else {
            continue;
        };
        if spec.encoding == FileEncoding::Base64 {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some(format!("request.body.value.{name}")),
                message: format!(
                    "`{value}` is base64-encoded, but a multipart field has no JSON position to inline it"
                ),
                hint: Some(
                    "use encoding: multipart, or move the file into a JSON body".to_string(),
                ),
            });
            continue;
        }
        let label = format!("files.{spec_name}");
        if let Some(file) = load_file(ctx, spec, &label, diagnostics) {
            files.push(FilePart {
                path: name.clone(),
                bytes: file.bytes,
                mime: file.mime,
                filename: file.filename,
            });
        }
    }

    // A file declared in the block but not named by the body map is still
    // uploaded, keyed by its own name — that is the field name in the common
    // case (`files: {avatar: …}` sends field `avatar`).
    for (name, spec) in resolver.files {
        if files.iter().any(|part| &part.path == name) || text.contains_key(name) {
            continue;
        }
        if spec.encoding == FileEncoding::Base64 {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some(format!("files.{name}")),
                message: "a base64 file has no JSON position in a multipart body".to_string(),
                hint: Some(
                    "use encoding: multipart, or move the file into a JSON body".to_string(),
                ),
            });
            continue;
        }
        let label = format!("files.{name}");
        if let Some(file) = load_file(ctx, spec, &label, diagnostics) {
            files.push(FilePart {
                path: name.clone(),
                bytes: file.bytes,
                mime: file.mime,
                filename: file.filename,
            });
        }
    }

    MultipartPlan::Rest {
        fields: text,
        files,
    }
}

/// A `rawFile` body: the whole body is the file's bytes, `Content-Type` is the
/// declared or sniffed MIME type, and no filename is sent (§7.14).
fn prepare_raw_file(
    ctx: &RunContext,
    resolver: &Resolver<'_>,
    text: &str,
    diagnostics: &mut Diagnostics,
) -> PreparedBody {
    let label = "request.body.value";

    // A whole `{{file.x}}` reference names a spec in the block, so a declared
    // MIME type survives; any other expression resolves to a path.
    if let Some(spec_name) = whole_file_ref(text) {
        let Some(spec) = resolver.files.get(&spec_name) else {
            return PreparedBody::bytes(None, None);
        };
        return match load_file(ctx, spec, label, diagnostics) {
            Some(file) => PreparedBody::bytes(Some(file.bytes), Some(&file.mime)),
            None => PreparedBody::bytes(None, None),
        };
    }

    let Some(path) = resolve_text(resolver, text, diagnostics) else {
        return PreparedBody::bytes(None, None);
    };
    let spec = FileSpec {
        path,
        mime_type: None,
        filename: None,
        encoding: FileEncoding::Multipart,
    };
    match load_file(ctx, &spec, label, diagnostics) {
        Some(file) => PreparedBody::bytes(Some(file.bytes), Some(&file.mime)),
        None => PreparedBody::bytes(None, None),
    }
}

/// Load one file through the run's cache, reporting a diagnostic under `path`.
fn load_file(
    ctx: &RunContext,
    spec: &FileSpec,
    path: &str,
    diagnostics: &mut Diagnostics,
) -> Option<LoadedFile> {
    let mut cache = ctx
        .files
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match cache.load(spec) {
        Ok(file) => Some(file),
        Err(mut diagnostic) => {
            diagnostic.path = Some(path.to_string());
            diagnostics.push(diagnostic);
            None
        }
    }
}

/// Replace every whole `{{file.x}}` string in a JSON body with the file's
/// base64 text, and refuse a multipart file where no part can be built.
fn inline_base64_files(
    ctx: &RunContext,
    resolver: &Resolver<'_>,
    value: &mut Value,
    path: &str,
    diagnostics: &mut Diagnostics,
) {
    match value {
        Value::String(text) => {
            let Some(spec_name) = whole_file_ref(text) else {
                return;
            };
            let Some(spec) = resolver.files.get(&spec_name) else {
                return;
            };
            if spec.encoding != FileEncoding::Base64 {
                diagnostics.push(Diagnostic {
                    code: codes::E_WRONG_TYPE,
                    severity: Severity::Error,
                    file: None,
                    span: None,
                    path: Some(path.to_string()),
                    message: format!(
                        "`{text}` is a multipart file, but a JSON body has no multipart part to put it in"
                    ),
                    hint: Some("use encoding: base64 to inline the file, or a multipart body".to_string()),
                });
                return;
            }
            let label = format!("files.{spec_name}");
            if let Some(file) = load_file(ctx, spec, &label, diagnostics) {
                *value = Value::String(STANDARD.encode(&file.bytes));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child = crate::template::value::join_path(path, &index.to_string());
                inline_base64_files(ctx, resolver, item, &child, diagnostics);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let child = crate::template::value::join_path(path, key);
                inline_base64_files(ctx, resolver, item, &child, diagnostics);
            }
        }
        _ => {}
    }
}

/// The name of a whole-value `{{file.x}}` reference, if `text` is exactly one.
///
/// A whole-value file reference resolves to its own token text (Task 10), which
/// is what lets this task tell a file position from ordinary text.
fn whole_file_ref(text: &str) -> Option<String> {
    let segments = scan(text).ok()?;
    match segments.as_slice() {
        [Segment::Token { inner, .. }] => match parse_token(inner).ok()? {
            Token::File(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

/// Parse an HTTP method, defaulting to GET so the diagnostic is the report.
fn parse_method(method: &str, diagnostics: &mut Diagnostics) -> reqwest::Method {
    match method_or_none(method) {
        Some(method) => method,
        None => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some("request.method".to_string()),
                message: format!("`{method}` is not a valid HTTP method"),
                hint: None,
            });
            reqwest::Method::GET
        }
    }
}

/// Parse a method without a diagnostic, for the error path that already has
/// one to report.
fn method_or_none(method: &str) -> Option<reqwest::Method> {
    let upper = method.to_ascii_uppercase();
    reqwest::Method::from_bytes(upper.as_bytes()).ok()
}

/// Resolve one string expression, expecting a string result.
fn resolve_text(
    resolver: &Resolver<'_>,
    text: &str,
    diagnostics: &mut Diagnostics,
) -> Option<String> {
    match resolver.resolve_str(text) {
        Ok(Value::String(resolved)) => Some(resolved),
        Ok(other) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!(
                    "`{text}` resolves to {}, expected a string",
                    type_name(&other)
                ),
                hint: None,
            });
            None
        }
        Err(errors) => {
            diagnostics.extend(errors.iter().cloned());
            None
        }
    }
}

/// Resolve a header value, where a number or boolean is acceptable text.
fn resolve_header_value(
    resolver: &Resolver<'_>,
    text: &str,
    diagnostics: &mut Diagnostics,
) -> Option<String> {
    match resolver.resolve_str(text) {
        Ok(Value::String(resolved)) => Some(resolved),
        Ok(Value::Number(number)) => Some(number.to_string()),
        Ok(Value::Bool(boolean)) => Some(boolean.to_string()),
        Ok(other) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!(
                    "header value `{text}` resolves to {}, expected a string",
                    type_name(&other)
                ),
                hint: None,
            });
            None
        }
        Err(errors) => {
            diagnostics.extend(errors.iter().cloned());
            None
        }
    }
}

/// Resolve a JSON value; on failure keep the unresolved value and report.
fn resolve_value(resolver: &Resolver<'_>, value: &Value, diagnostics: &mut Diagnostics) -> Value {
    match resolver.resolve(value) {
        Ok(resolved) => resolved,
        Err(errors) => {
            diagnostics.extend(errors.iter().cloned());
            value.clone()
        }
    }
}

/// Check a header name and value before they reach `reqwest`, which panics on
/// invalid ones when a `RequestBuilder` is built.
fn validate_header(name: &str, value: &str, diagnostics: &mut Diagnostics) -> bool {
    if let Err(error) = HeaderName::from_bytes(name.as_bytes()) {
        diagnostics.push(Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: format!("`{name}` is not a valid header name: {error}"),
            hint: None,
        });
        return false;
    }
    if let Err(error) = HeaderValue::from_str(value) {
        diagnostics.push(Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: format!("`{name}` has a value that is not valid in a header: {error}"),
            hint: None,
        });
        return false;
    }
    true
}

/// Encode a JSON value as body bytes.
fn encode_json(value: &Value, diagnostics: &mut Diagnostics) -> Option<Bytes> {
    match serde_json::to_vec(value) {
        Ok(bytes) => Some(Bytes::from(bytes)),
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!("could not encode the JSON body: {error}"),
                hint: None,
            });
            None
        }
    }
}

/// Encode a flat form body, resolving each value.
fn encode_form(
    fields: &BTreeMap<String, String>,
    resolver: &Resolver<'_>,
    diagnostics: &mut Diagnostics,
) -> String {
    let mut out = String::new();
    for (name, raw) in fields {
        if !out.is_empty() {
            out.push('&');
        }
        let value = resolve_text(resolver, raw, diagnostics).unwrap_or_else(|| raw.clone());
        out.push_str(&utf8_percent_encode(name, FORM_ENCODE).to_string());
        out.push('=');
        out.push_str(&utf8_percent_encode(&value, FORM_ENCODE).to_string());
    }
    out
}

/// A human name for a JSON value's type, for diagnostics.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Send a prepared request through `client`, retrying transport failures at
/// most `resolved.retries` times, 100 ms apart. A 4xx/5xx response is never
/// retried.
async fn send(ctx: &RunContext, request: PreparedRequest, client: &reqwest::Client) -> Outcome {
    let started = Instant::now();

    let mut attempt = 0;
    loop {
        if ctx.cancel.is_cancelled() {
            return transport_failure(
                ctx,
                &request,
                TransportKind::Cancelled,
                "cancelled".to_string(),
                started.elapsed(),
            );
        }

        ctx.emit(Event::Sent {
            bytes: request.body.as_ref().map_or(0, |body| body.len() as u64),
        });

        match send_once(ctx, client, &request, started).await {
            Ok(outcome) => return outcome,
            Err((kind, message)) => {
                if is_retryable(&kind) && attempt < ctx.resolved.retries {
                    attempt += 1;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                return transport_failure(ctx, &request, kind, message, started.elapsed());
            }
        }
    }
}

/// One network attempt: build, send, read status/headers/cookies/body.
async fn send_once(
    ctx: &RunContext,
    client: &reqwest::Client,
    request: &PreparedRequest,
    started: Instant,
) -> Result<Outcome, (TransportKind, String)> {
    let wire = build_wire_request(client, request)?;

    let response = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            return Err((TransportKind::Cancelled, "cancelled".to_string()));
        }
        response = client.execute(wire) => response.map_err(classify)?,
    };

    let status = response.status().as_u16();
    let headers = response_headers(response.headers());
    let cookies = response_cookies(response.headers(), &request.url);

    let body = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            return Err((TransportKind::Cancelled, "cancelled".to_string()));
        }
        body = response.bytes() => body.map_err(classify)?,
    };

    let body_size = body.len() as u64;
    let elapsed = started.elapsed();
    ctx.emit(Event::Received {
        status,
        bytes: body_size,
        elapsed,
    });

    Ok(Outcome {
        operation: String::new(),
        user: ctx.user.clone(),
        request: request_echo(request),
        status,
        headers,
        cookies,
        body: ctx.retain_body.then_some(body),
        body_retained: ctx.retain_body,
        body_size,
        graphql_errors: Vec::new(),
        timing: Timing {
            total: elapsed,
            connect: None,
            ttfb: None,
        },
        transport_error: None,
    })
}

/// Turn a [`PreparedRequest`] into a `reqwest::Request`, applying the body's
/// default `Content-Type` only when no merged header set one.
fn build_wire_request(
    client: &reqwest::Client,
    request: &PreparedRequest,
) -> Result<reqwest::Request, (TransportKind, String)> {
    let mut builder = client.request(request.method.clone(), request.url.clone());

    // A multipart body owns its `Content-Type`, boundary included: a header
    // copied in here would replace it and produce an unparseable body (§7.6).
    let multipart = request.multipart.is_some();
    let mut content_type_set = false;
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-type") {
            content_type_set = true;
            if multipart {
                continue;
            }
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(content_type) = &request.content_type
        && !content_type_set
    {
        builder = builder.header(CONTENT_TYPE, content_type.as_str());
    }
    if let Some(plan) = &request.multipart {
        builder = builder.multipart(form_for(plan));
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    builder
        .build()
        .map_err(|error| (TransportKind::Other, error.to_string()))
}

/// Assemble the multipart form for one attempt, boundary included.
fn form_for(plan: &MultipartPlan) -> reqwest::multipart::Form {
    match plan {
        MultipartPlan::Graphql { body, files } => {
            let tuples: Vec<_> = files.iter().map(FilePart::as_tuple).collect();
            build_graphql_multipart(body, &tuples)
        }
        MultipartPlan::Rest { fields, files } => {
            let tuples: Vec<_> = files.iter().map(FilePart::as_tuple).collect();
            let mut form = build_rest_multipart(&tuples);
            for (name, value) in fields {
                form = form.text(name.clone(), value.clone());
            }
            form
        }
    }
}

/// The response headers in wire order.
fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().map_or_else(
                |_| String::from_utf8_lossy(value.as_bytes()).into_owned(),
                str::to_string,
            );
            (name.as_str().to_string(), value)
        })
        .collect()
}

/// Derive cookies from the response's own `Set-Cookie` headers, defaulting
/// domain to the request host and path to `/`. A malformed header is skipped.
fn response_cookies(headers: &reqwest::header::HeaderMap, url: &Url) -> Vec<CookieInfo> {
    let host = url.host_str().unwrap_or_default();
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| parse_set_cookie(value, host))
        .collect()
}

/// Parse one `Set-Cookie` value; `None` when there is no `name=value` pair.
fn parse_set_cookie(text: &str, host: &str) -> Option<CookieInfo> {
    let mut parts = text.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let mut cookie = CookieInfo {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain: host.to_string(),
        path: "/".to_string(),
        http_only: false,
        secure: false,
        expires: None,
    };

    for attribute in parts {
        let attribute = attribute.trim();
        let (key, value) = match attribute.split_once('=') {
            Some((key, value)) => (key.trim(), Some(value.trim())),
            None => (attribute, None),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" => {
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    cookie.domain = value.to_string();
                }
            }
            "path" => {
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    cookie.path = value.to_string();
                }
            }
            "httponly" => cookie.http_only = true,
            "secure" => cookie.secure = true,
            "expires" => {
                if let Some(value) = value {
                    cookie.expires = Some(value.to_string());
                }
            }
            _ => {}
        }
    }

    Some(cookie)
}

/// The request echo, exactly what was sent.
fn request_echo(request: &PreparedRequest) -> RequestEcho {
    RequestEcho {
        method: request.method.as_str().to_string(),
        url: request.url.to_string(),
        headers: request.headers.clone(),
        body_size: request.body.as_ref().map_or(0, |body| body.len() as u64),
    }
}

/// A transport failure as an outcome: `status: 0`, no body, one `Failed` event.
fn transport_failure(
    ctx: &RunContext,
    request: &PreparedRequest,
    kind: TransportKind,
    message: String,
    total: Duration,
) -> Outcome {
    ctx.emit(Event::Failed {
        diagnostic: Diagnostic {
            code: codes::E_TRANSPORT,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: message.clone(),
            hint: Some(format!(
                "the request did not complete; category `{}`",
                kind_label(&kind)
            )),
        },
    });

    Outcome {
        operation: String::new(),
        user: ctx.user.clone(),
        request: request_echo(request),
        status: 0,
        headers: Vec::new(),
        cookies: Vec::new(),
        body: None,
        body_retained: ctx.retain_body,
        body_size: 0,
        graphql_errors: Vec::new(),
        timing: Timing {
            total,
            connect: None,
            ttfb: None,
        },
        transport_error: Some(TransportError { kind, message }),
    }
}

/// A template or session failure: no request is sent, every diagnostic
/// becomes a `Failed` event, and the outcome carries the rendered text as an
/// `Other` transport error.
pub(crate) fn failure_outcome(
    ctx: &RunContext,
    operation: &Operation,
    diagnostics: &Diagnostics,
) -> Outcome {
    for diagnostic in diagnostics.iter() {
        ctx.emit(Event::Failed {
            diagnostic: diagnostic.clone(),
        });
    }

    let (method, path) = match &operation.request {
        Request::Graphql(request) => (
            reqwest::Method::POST,
            request
                .path
                .clone()
                .unwrap_or_else(|| ctx.resolved.graphql_path.clone()),
        ),
        Request::Http(request) => (
            method_or_none(&request.method).unwrap_or(reqwest::Method::GET),
            request.path.clone(),
        ),
    };

    let mut outcome = Outcome {
        operation: operation.id.clone(),
        user: ctx.user.clone(),
        request: RequestEcho {
            method: method.as_str().to_string(),
            url: ctx.resolved.url_for(&path).to_string(),
            headers: Vec::new(),
            body_size: 0,
        },
        status: 0,
        headers: Vec::new(),
        cookies: Vec::new(),
        body: None,
        body_retained: ctx.retain_body,
        body_size: 0,
        graphql_errors: Vec::new(),
        timing: Timing {
            total: Duration::ZERO,
            connect: None,
            ttfb: None,
        },
        transport_error: Some(TransportError {
            kind: TransportKind::Other,
            message: render_text(diagnostics),
        }),
    };

    outcome.redact(&ctx.redactor);
    ctx.emit(Event::Finished {
        outcome: outcome.clone(),
    });
    outcome
}

/// Classify a `reqwest` failure per the transport-error table: connect,
/// timeout, body/read, TLS (message text), else other.
fn classify(error: reqwest::Error) -> (TransportKind, String) {
    let message = error.to_string();
    let kind = if error.is_connect() {
        TransportKind::Connect
    } else if error.is_timeout() {
        TransportKind::Timeout
    } else if error.is_body() {
        TransportKind::Read
    } else if looks_like_tls(&error) {
        TransportKind::Tls
    } else {
        TransportKind::Other
    };
    (kind, message)
}

/// Whether the error text (including its source chain) reads as a TLS problem.
fn looks_like_tls(error: &reqwest::Error) -> bool {
    let mut text = error.to_string().to_ascii_lowercase();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push(' ');
        text.push_str(&cause.to_string().to_ascii_lowercase());
        source = cause.source();
    }
    ["tls", "certificate", "handshake", "ssl"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// Only transport failures retry: connect, read and timeout. TLS and other
/// failures are deterministic; 4xx/5xx are responses and are never retried.
fn is_retryable(kind: &TransportKind) -> bool {
    matches!(
        kind,
        TransportKind::Connect | TransportKind::Read | TransportKind::Timeout
    )
}

/// A stable lowercase name for a transport failure category.
fn kind_label(kind: &TransportKind) -> &'static str {
    match kind {
        TransportKind::Connect => "connect",
        TransportKind::Timeout => "timeout",
        TransportKind::Read => "read",
        TransportKind::Tls => "tls",
        TransportKind::Cancelled => "cancelled",
        TransportKind::Other => "other",
    }
}
