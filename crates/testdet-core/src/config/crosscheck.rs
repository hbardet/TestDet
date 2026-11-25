//! Cross-file validation: logins, references and files (§7.13).
//!
//! Runs after config load over every operation. It resolves template
//! references at the *config* level only — no network, no secret values — and
//! reports every problem (no early return), sorted by `(file, line, column)`.
//!
//! The scan surface per operation is exactly the §7.13 cross-file list:
//! the graphql `query` and `variables`, the http `path` and `body`, each
//! `headers` value that is `Some`, `vars` values, and `capture` values. The
//! operation's `id`, `description`, `tags` and `files` metadata are *not*
//! scanned: a file's own `path`/`filename` are filesystem names, not templates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::config::discover::Config;
use crate::config::model::{HeaderMap, User};
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, Span, codes};
use crate::operation::model::{Body, Operation, Request};
use crate::template::value::{Reference, Token, malformed, references};

/// Validate every cross-file rule over the whole config.
///
/// Errors and warnings are collected for every operation (and every
/// `defaultLogin` / per-user `login` override) before anything is sorted or
/// returned, so a single bad reference never hides another.
#[must_use]
pub fn crosscheck(config: &Config) -> Diagnostics {
    let mut out = Diagnostics::new();

    check_default_login(config, &mut out);
    check_per_user_logins(config, &mut out);

    for surface in reference_surfaces(config) {
        check_value_in(
            &surface.value,
            &surface.owner,
            &surface.file,
            &surface.path_prefix,
            config,
            &mut out,
        );
    }

    for (id, operation) in &config.operations {
        check_unused_files(id, operation, &scan_surface(operation), config, &mut out);
    }

    sort(&mut out);
    out
}

/// The `used_by` owner recorded for a reference made from `users.yaml`.
///
/// `required_vars`, crosscheck and the redaction seed all read the same
/// surface, so they agree on this spelling.
pub const USERS_SURFACE: &str = "users.yaml";

/// One template-bearing surface of the config: the synthetic JSON document to
/// scan, the owner named in diagnostics, the file it was declared in and the
/// document-path prefix for its fields.
pub(crate) struct ReferenceSurface {
    /// The diagnostic owner: an operation id, or [`USERS_SURFACE`].
    pub owner: String,
    /// The file the surface was declared in, when known.
    pub file: Option<PathBuf>,
    /// Prepended to each reference path (`""` for operations,
    /// `"users.<name>."` for a user entry).
    pub path_prefix: String,
    /// The synthetic document scanned for references.
    pub value: Value,
}

/// Every template-bearing surface of the config, in a stable order.
///
/// This is the single source of truth for "which references does this config
/// make": the cross-file rules, [`crate::secrets::required_vars`] and the
/// redaction seed all iterate it, so a secret used only from `users.yaml` can
/// never be invisible to one of them (§7.11, §7.12).
pub(crate) fn reference_surfaces(config: &Config) -> Vec<ReferenceSurface> {
    let mut surfaces = Vec::new();

    for (id, operation) in &config.operations {
        surfaces.push(ReferenceSurface {
            owner: id.clone(),
            file: config.operation_files.get(id).cloned(),
            path_prefix: String::new(),
            value: scan_surface(operation),
        });
    }

    if !config.users.is_empty() {
        let users_file = config.root_dir.join(USERS_SURFACE);
        for (name, user) in &config.users {
            surfaces.push(ReferenceSurface {
                owner: USERS_SURFACE.to_string(),
                file: Some(users_file.clone()),
                path_prefix: format!("users.{name}."),
                value: user_surface(user),
            });
        }
    }

    surfaces
}

/// The template-bearing fields of one user entry, as a JSON object whose keys
/// are the on-disk names (`token`, `tokenHeader`, `credentials`, `headers`,
/// `vars`).
fn user_surface(user: &User) -> Value {
    let mut doc = serde_json::Map::new();
    if let Some(Some(token)) = &user.token {
        doc.insert("token".to_string(), Value::String(token.clone()));
    }
    if let Some(Some(header)) = &user.token_header {
        doc.insert("tokenHeader".to_string(), Value::String(header.clone()));
    }
    doc.insert("credentials".to_string(), strings_object(&user.credentials));
    doc.insert("headers".to_string(), headers_object(&user.headers));
    doc.insert("vars".to_string(), json_object(&user.vars));
    Value::Object(doc)
}

/// Validate every reference found in `value` for the operation named `owner`.
///
/// Each diagnostic is tagged with the reference's document `path` and names
/// the owner operation in its message. `owner` is the operation id.
pub fn check_value(value: &Value, owner: &str, config: &Config, out: &mut Diagnostics) {
    let file = config.operation_files.get(owner).cloned();
    check_value_in(value, owner, &file, "", config, out);
}

/// The shared body of [`check_value`], with an explicit file and path prefix
/// so `users.yaml` fields report `users.<name>.<key>` paths.
fn check_value_in(
    value: &Value,
    owner: &str,
    file: &Option<PathBuf>,
    path_prefix: &str,
    config: &Config,
    out: &mut Diagnostics,
) {
    for reference in references(value) {
        check_reference(&reference, owner, file, path_prefix, config, out);
    }
    for occurrence in malformed(value) {
        let path = format!("{path_prefix}{}", occurrence.path);
        out.push(Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file: file.clone(),
            span: span_for_reference(config, file, &path),
            path: Some(path),
            message: format!(
                "malformed template `{raw}` in `{owner}`: {reason}",
                raw = occurrence.raw,
                reason = occurrence.reason
            ),
            hint: None,
        });
    }
}

/// Apply the cross-file rules to one well-formed reference.
fn check_reference(
    reference: &Reference,
    owner: &str,
    file: &Option<PathBuf>,
    path_prefix: &str,
    config: &Config,
    out: &mut Diagnostics,
) {
    let operation = config.operations.get(owner);
    let path = format!("{path_prefix}{}", reference.path);
    let span = span_for_reference(config, file, &path);
    match &reference.token {
        Token::File(name) => {
            let declared = operation.is_some_and(|op| op.files.contains_key(name));
            if !declared {
                out.push(Diagnostic {
                    code: codes::E_UNKNOWN_FILE,
                    severity: Severity::Error,
                    file: file.clone(),
                    span,
                    path: Some(path),
                    message: format!("file `{name}` is referenced but not declared in `{owner}`"),
                    hint: Some(file_names_hint(operation)),
                });
            }
        }
        Token::Env(name) => {
            // §7.11: a missing declaration file treats every reference as
            // required, so a reference is only an error when a declaration
            // file exists and omits the name.
            let undeclared = config
                .secrets
                .as_ref()
                .is_some_and(|secrets| !secrets.0.contains_key(name));
            if undeclared {
                out.push(Diagnostic {
                    code: codes::E_UNKNOWN_ENV,
                    severity: Severity::Error,
                    file: file.clone(),
                    span,
                    path: Some(path),
                    message: format!("secret `{name}` is not declared in `{owner}`"),
                    hint: Some(secrets_file_hint(name, config)),
                });
            }
        }
        Token::Session(_name) => {
            check_session(reference, owner, file, config, out, &path, &span);
        }
        Token::Var(_) => {
            // `var` resolution is a within-config concern (operation/env/user
            // layers), owned by Task 10 and the runtime — not cross-file.
        }
    }
}

/// `defaultLogin` must name an existing operation id.
fn check_default_login(config: &Config, out: &mut Diagnostics) {
    let Some(login) = &config.default_login else {
        return;
    };
    if config.operations.contains_key(login) {
        return;
    }
    out.push(Diagnostic {
        code: codes::E_UNRESOLVED_REF,
        severity: Severity::Error,
        file: Some(config.root_dir.join("users.yaml")),
        span: None,
        path: Some("defaultLogin".to_string()),
        message: format!("`defaultLogin` names `{login}`, which is not an operation"),
        hint: Some(operations_hint(config)),
    });
}

/// Every `users.<name>.login` override must name an existing operation id.
fn check_per_user_logins(config: &Config, out: &mut Diagnostics) {
    let users_file = config.root_dir.join("users.yaml");
    for (name, user) in &config.users {
        let Some(login) = user.login.as_ref().and_then(|login| login.as_deref()) else {
            continue;
        };
        if config.operations.contains_key(login) {
            continue;
        }
        out.push(Diagnostic {
            code: codes::E_UNRESOLVED_REF,
            severity: Severity::Error,
            file: Some(users_file.clone()),
            span: None,
            path: Some(format!("users.{name}.login")),
            message: format!("`users.{name}.login` names `{login}`, which is not an operation"),
            hint: Some(operations_hint(config)),
        });
    }
}

/// Validate one `{{session.x}}` reference against the union of every user's
/// login, allowing it when *any* user's login satisfies it.
fn check_session(
    reference: &Reference,
    owner: &str,
    file: &Option<PathBuf>,
    config: &Config,
    out: &mut Diagnostics,
    path: &str,
    span: &Option<Span>,
) {
    let Token::Session(name) = &reference.token else {
        return;
    };
    // Effective login id per user: `user.login` overrides `defaultLogin`.
    let mut login_users: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (user_name, user) in &config.users {
        if let Some(login) = user
            .login
            .clone()
            .flatten()
            .or_else(|| config.default_login.clone())
        {
            login_users
                .entry(login)
                .or_default()
                .push(user_name.clone());
        }
    }

    // Nothing known at all → a session reference cannot be validated.
    if login_users.is_empty() {
        out.push(Diagnostic {
            code: codes::E_UNKNOWN_VAR,
            severity: Severity::Error,
            file: file.clone(),
            span: *span,
            path: Some(path.to_string()),
            message: format!(
                "`{raw}` in `{owner}` references a session that no login provides",
                raw = reference.raw
            ),
            hint: Some("declare a users file with a login operation first".to_string()),
        });
        return;
    }

    // During login (owner is itself a login operation), a session name is
    // legal when it names a credential of any user that logs in through it.
    if let Some(user_names) = login_users.get(owner) {
        for user_name in user_names {
            if let Some(user) = config.users.get(user_name)
                && credential_matches(user, name)
            {
                return;
            }
        }
    }

    // Otherwise the name must be captured by some login operation.
    for login in login_users.keys() {
        if login == owner {
            continue;
        }
        if config
            .operations
            .get(login)
            .is_some_and(|op| op.capture.contains_key(name))
        {
            return;
        }
    }

    let login_list = backticked(login_users.keys());
    let mut capture_names = BTreeSet::new();
    for login in login_users.keys() {
        if let Some(op) = config.operations.get(login) {
            capture_names.extend(op.capture.keys().cloned());
        }
    }

    let hint = if capture_names.is_empty() {
        "declare it in the login operation's `capture:`".to_string()
    } else {
        let list = capture_names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("captured by {login_list}: {list}")
    };

    out.push(Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: file.clone(),
        span: *span,
        path: Some(path.to_string()),
        message: format!(
            "`{raw}` in `{owner}` is not captured by the login operation {login_list}",
            raw = reference.raw
        ),
        hint: Some(hint),
    });
}

/// Whether a session name refers to a credential during login: either the bare
/// credentials key (`{{session.email}}`) or the prefixed form
/// (`{{session.credentials.email}}`).
fn credential_matches(user: &User, name: &str) -> bool {
    if user.credentials.contains_key(name) {
        return true;
    }
    name.strip_prefix("credentials.")
        .is_some_and(|key| user.credentials.contains_key(key))
}

/// The source span for a reference path, resolved against the owning
/// operation's document. Scan-surface paths (`variables.email`, `body.value.x`,
/// `headers.Authorization`) are mapped onto the on-disk document (`request.…`)
/// before lookup; the lookup then walks up to parent paths until a span is
/// found (e.g. `request.variables.avatar` → `request.variables`).
fn span_for_reference(
    config: &Config,
    file: &Option<PathBuf>,
    reference_path: &str,
) -> Option<Span> {
    let file = file.as_ref()?;
    let document = config.documents.get(file)?;
    let mut path = on_disk_path(reference_path);
    loop {
        if let Some(span) = document.span_at(&path) {
            return Some(span);
        }
        let index = path.rfind('.')?;
        path.truncate(index);
    }
}

/// Map a scan-surface reference path onto the on-disk document path. Request
/// content is wrapped under `request.` in the operation file; `vars` and
/// `capture` are top-level and match directly.
fn on_disk_path(path: &str) -> String {
    if path.starts_with("request.") {
        return path.to_string();
    }
    match path.split('.').next().unwrap_or(path) {
        "query" | "variables" | "headers" | "path" | "body" | "method" => format!("request.{path}"),
        _ => path.to_string(),
    }
}

/// Warn for a `files` entry of an operation that no `{{file.*}}` reference in
/// that operation uses.
fn check_unused_files(
    id: &str,
    operation: &Operation,
    surface: &Value,
    config: &Config,
    out: &mut Diagnostics,
) {
    let mut used = BTreeSet::new();
    for reference in references(surface) {
        if let Token::File(name) = &reference.token {
            used.insert(name.clone());
        }
    }

    let file = config.operation_files.get(id).cloned();
    for name in operation.files.keys() {
        if !used.contains(name) {
            let span = file
                .as_ref()
                .and_then(|file| config.documents.get(file))
                .and_then(|doc| doc.span_at(&format!("files.{name}")));
            out.push(Diagnostic {
                code: codes::W_UNUSED_VAR,
                severity: Severity::Warning,
                file: file.clone(),
                span,
                path: Some(format!("files.{name}")),
                message: format!("file `{name}` is declared but never referenced"),
                hint: None,
            });
        }
    }
}

/// Build the synthetic document that is the per-operation scan surface, so
/// every reference carries a document path (`query`, `variables.email`,
/// `body.value.x`, `headers.Authorization`, `vars.userId`, `capture.token`).
pub(crate) fn scan_surface(operation: &Operation) -> Value {
    let mut doc = serde_json::Map::new();

    match &operation.request {
        Request::Graphql(request) => {
            doc.insert("query".to_string(), Value::String(request.query.clone()));
            doc.insert("variables".to_string(), json_object(&request.variables));
            doc.insert("headers".to_string(), headers_object(&request.headers));
        }
        Request::Http(request) => {
            doc.insert("path".to_string(), Value::String(request.path.clone()));
            if let Some(body) = &request.body {
                doc.insert("body".to_string(), body_value(body));
            }
            doc.insert("headers".to_string(), headers_object(&request.headers));
        }
    }

    doc.insert("vars".to_string(), json_object(&operation.vars));
    doc.insert("capture".to_string(), strings_object(&operation.capture));

    Value::Object(doc)
}

/// A request body's scannable value. The document key is always `value`
/// (`body: { kind: json, value: ... }`, §7.6), so the content is wrapped
/// under `value` to make reference paths match the on-disk document:
/// `body.value.email`, `body.value.<form-field>`, `body.value`.
fn body_value(body: &Body) -> Value {
    let content = match body {
        Body::Json(value) => value.clone(),
        Body::Text(text) => Value::String(text.clone()),
        Body::Form(map) | Body::Multipart(map) => strings_object(map),
        Body::RawFile(path) => Value::String(path.clone()),
    };
    serde_json::json!({ "value": content })
}

/// `BTreeMap<String, Value>` → JSON object, preserving key order.
fn json_object(map: &BTreeMap<String, Value>) -> Value {
    Value::Object(
        map.iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

/// `BTreeMap<String, String>` → JSON object of strings.
fn strings_object(map: &BTreeMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// `HeaderMap` → JSON object containing only the `Some` values (a `None` value
/// means "unset this header" and carries no template to scan).
fn headers_object(headers: &HeaderMap) -> Value {
    Value::Object(
        headers
            .0
            .iter()
            .filter_map(|(name, value)| {
                value
                    .as_ref()
                    .map(|value| (name.clone(), Value::String(value.clone())))
            })
            .collect(),
    )
}

/// List the declared file names of `operation`, sorted (BTreeMap key order).
fn file_names_hint(operation: Option<&Operation>) -> String {
    let Some(operation) = operation else {
        return "this operation declares no files".to_string();
    };
    if operation.files.is_empty() {
        return "this operation declares no files".to_string();
    }
    let names = operation
        .files
        .keys()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("declared files: {names}")
}

/// The `E_UNKNOWN_ENV` hint names the secrets declaration file for the
/// environment the declaration was loaded from, falling back to the synthetic
/// `local` environment.
fn secrets_file_hint(name: &str, config: &Config) -> String {
    let env = &config.secrets_env;
    format!("declare `{name}` in testdet/secrets/{env}.yaml")
}

/// The `E_UNRESOLVED_REF` hint lists the existing operation ids, sorted, and
/// truncated to the first 8 followed by `, …`.
fn operations_hint(config: &Config) -> String {
    let names: Vec<&String> = config.operations.keys().collect();
    if names.is_empty() {
        return "no operations declared".to_string();
    }

    let mut list = String::new();
    for (index, name) in names.iter().take(8).enumerate() {
        if index > 0 {
            list.push_str(", ");
        }
        list.push('`');
        list.push_str(name);
        list.push('`');
    }
    if names.len() > 8 {
        list.push_str(", …");
    }
    format!("known operations: {list}")
}

/// Backtick-quote each key and join with `, `.
fn backticked<'a>(keys: impl IntoIterator<Item = &'a String>) -> String {
    keys.into_iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sort diagnostics by `(file, line, column)` with absent locations first,
/// matching the config loader's deterministic ordering.
fn sort(diagnostics: &mut Diagnostics) {
    let mut items: Vec<Diagnostic> = diagnostics.iter().cloned().collect();
    items.sort_by(|a, b| {
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
    *diagnostics = Diagnostics::from(items);
}
