//! The per-user session store (§7.7).
//!
//! One [`Session`] per user — a `reqwest::Client` with its own cookie jar, the
//! captured `{{session.*}}` values, session default headers (a static token)
//! and a `logged_in` flag. The anonymous run is a session keyed by `None`, so
//! the context's `user` stays a single `Option<String>` throughout the engine.
//!
//! Sessions are in-memory only (§7.7.7): nothing is persisted and `testdet
//! run` logs in afresh on every invocation.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use reqwest::cookie::Jar;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::config::Resolved;
use crate::context::RunContext;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::events::Event;
use crate::operation::model::Operation;
use crate::outcome::Outcome;
use crate::secrets::build_source;
use crate::template::Resolver;

use super::login::login_operation;

/// One user's live session.
///
/// `jar` is kept beside the client (which holds its own `Arc` to it) so a
/// future frontend can inspect or discard cookies explicitly; the engine never
/// enumerates it. `headers` holds the session's default headers — today only
/// the `token:` sugar — and is merged at the user layer of every request.
pub(crate) struct Session {
    client: reqwest::Client,
    #[allow(dead_code)] // kept for explicit lifetime/ownership; see the doc
    jar: Arc<Jar>,
    vars: BTreeMap<String, Value>,
    headers: Vec<(String, String)>,
    logged_in: bool,
}

/// Every user's session, plus the [`RunContext`] the engine reads.
///
/// Construct with [`SessionStore::new`]; sessions are created lazily and
/// reused, so switching back to an already-authenticated user sends no login
/// request (§7.7.2).
pub struct SessionStore {
    ctx: RunContext,
    sessions: HashMap<Option<String>, Session>,
    /// Returned by [`SessionStore::vars`] for an unknown/absent session, so the
    /// accessor is total and never panics.
    empty_vars: BTreeMap<String, Value>,
    /// The operation whose response is being captured, so a capture diagnostic
    /// can name the operation even though `capture_into` does not take it.
    active_operation: Option<String>,
}

impl SessionStore {
    /// A store over `ctx`. The context's `user` is the default identity used by
    /// [`crate::engine::execute`]; [`super::execute_for_user`] overrides it.
    #[must_use]
    pub fn new(ctx: RunContext) -> Self {
        Self {
            ctx,
            sessions: HashMap::new(),
            empty_vars: BTreeMap::new(),
            active_operation: None,
        }
    }

    /// The identity [`crate::engine::execute`] runs as.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.ctx.user.as_deref()
    }

    /// Make sure `user` is ready to send requests: validate the name, build the
    /// session lazily, and authenticate when needed.
    ///
    /// State machine (§7.7):
    /// - unknown user name → `E_UNKNOWN_USER` (hint lists the known users);
    /// - already `logged_in` → nothing (switching back is free);
    /// - `token:` → the header is installed in the session, no request is sent;
    /// - `login:` (or `defaultLogin`) → run that operation once in this session,
    ///   capture its `capture:` map, register the captured secrets;
    /// - neither → nothing to authenticate.
    ///
    /// A login that fails (transport error or auth failure) returns a
    /// diagnostic and leaves `logged_in == false`. The login itself is never
    /// retried here — the single retry lives in
    /// [`super::execute_for_user`].
    // The `Err` type is the pinned interface; boxing it would leak into every
    // caller of `execute_for_user`.
    #[allow(clippy::result_large_err)]
    pub async fn ensure(&mut self, user: Option<&str>) -> Result<(), Diagnostic> {
        if let Some(diagnostic) = self.check_user(user) {
            return Err(diagnostic);
        }

        if self.is_logged_in(user) {
            return Ok(());
        }

        self.open(user);

        // A static token is a header, not a round-trip.
        if self.has_static_token(user) {
            self.mark_logged_in(user);
            return Ok(());
        }

        let Some(login_id) = login_operation(&self.ctx.resolved, user) else {
            // No credentials, no token, no login: nothing to authenticate.
            self.mark_logged_in(user);
            return Ok(());
        };

        let Some(operation) = self.ctx.resolved.operations.get(&login_id).cloned() else {
            return Err(Diagnostic {
                code: codes::E_UNRESOLVED_REF,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!(
                    "user `{}` names login operation `{login_id}`, which does not exist",
                    user_name(user)
                ),
                hint: Some("`login`/`defaultLogin` must name an operation id".to_string()),
            });
        };

        // Run the login in the same session so `{{session.credentials.*}}`
        // resolves and cookies from its response land in the right jar. The
        // body is forced retained: `capture:` needs it.
        let outcome = self
            .run(user, &operation, !operation.capture.is_empty())
            .await;

        // A login that 500s, answers with GraphQL errors or never reaches the
        // server has not authenticated anyone: do not mark the session, and
        // let the caller's exit code reflect the diagnostic (§7.7, F3).
        if !login_succeeded(&outcome) {
            return Err(login_failure(user, &login_id, &outcome));
        }

        self.mark_logged_in(user);
        Ok(())
    }

    /// The captured variables for `user`, or an empty map when the session does
    /// not exist. Never panics (§7.7/§7.8).
    #[must_use]
    pub fn vars(&self, user: Option<&str>) -> &BTreeMap<String, Value> {
        self.sessions
            .get(&session_key(user))
            .map_or(&self.empty_vars, |session| &session.vars)
    }

    /// The client for `user`, creating the session lazily. A build failure
    /// falls back to a default client rather than panicking.
    pub fn client(&mut self, user: Option<&str>) -> &reqwest::Client {
        let key = session_key(user);
        if !self.sessions.contains_key(&key) {
            let session = self.build_session(user);
            self.sessions.insert(key.clone(), session);
        }
        match self.sessions.get(&key) {
            Some(session) => &session.client,
            None => fallback_client(),
        }
    }

    /// Capture `capture`'s values from a JSON `body` into `user`'s session.
    ///
    /// Every captured value lands in `vars` under its `name`; string values are
    /// also registered with the shared redactor as `CAPTURE_<NAME>` (§7.12).
    /// A miss is one `E_UNRESOLVED_CAPTURE` `Failed` event and does **not** stop
    /// the other captures. A non-JSON body is one diagnostic and captures
    /// nothing.
    pub fn capture_into(
        &mut self,
        user: Option<&str>,
        capture: &BTreeMap<String, String>,
        body: &[u8],
    ) {
        if capture.is_empty() {
            return;
        }

        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                let diagnostic = Diagnostic {
                    code: codes::E_UNRESOLVED_CAPTURE,
                    severity: Severity::Error,
                    file: None,
                    span: None,
                    path: None,
                    message: "capture needs a JSON body".to_string(),
                    hint: Some(format!(
                        "operation `{}` declared a `capture:` block",
                        self.active_operation.as_deref().unwrap_or("<unknown>")
                    )),
                };
                self.ctx.emit(Event::Failed { diagnostic });
                return;
            }
        };

        self.open(user);

        let mut captured = Vec::new();
        let mut misses = Vec::new();
        for (name, path) in capture {
            match resolve_capture_path(&value, path) {
                Some(value) => captured.push((name.clone(), value)),
                None => misses.push(path.clone()),
            }
        }

        for (name, value) in &captured {
            if let Value::String(secret) = value
                && !secret.contains("-CENSORED")
            {
                self.ctx
                    .redactor
                    .register(&format!("CAPTURE_{}", name.to_uppercase()), secret);
            }
        }

        if let Some(session) = self.sessions.get_mut(&session_key(user)) {
            for (name, value) in captured {
                session.vars.insert(name, value);
            }
        }

        for path in misses {
            let diagnostic = Diagnostic {
                code: codes::E_UNRESOLVED_CAPTURE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!("capture path `{path}` did not resolve"),
                hint: Some(format!(
                    "operation `{}` captured from a response body without that path",
                    self.active_operation.as_deref().unwrap_or("<unknown>")
                )),
            };
            self.ctx.emit(Event::Failed { diagnostic });
        }
    }

    /// Replace the store's cancellation token with a fresh one and return it.
    ///
    /// The sessions are untouched: cookies, captured variables and default
    /// headers stay, so cancelling one request does not log the user out
    /// (§7.7, §7.10). See [`RunContext::renew_cancellation`].
    pub fn renew_cancellation(&mut self) -> tokio_util::sync::CancellationToken {
        self.ctx.renew_cancellation()
    }

    /// Run `operation` as `user` through that user's session client.
    ///
    /// `retain_body` forces body retention for this call. A `capture:` map
    /// forces it too: capturing is impossible without the body, so `--no-body`
    /// cannot make a capturing operation silently capture nothing (§7.9, F9).
    /// The client and variable/header maps are cloned before the first `await`,
    /// so no `self` borrow is held across it (requirement 8).
    ///
    /// Captures happen here, from the RAW body, before redaction: a captured
    /// token must be what the server sent, never `<CAPTURE_TOKEN>-CENSORED`
    /// (F2). The returned outcome is already redacted.
    pub(crate) async fn run(
        &mut self,
        user: Option<&str>,
        operation: &Operation,
        retain_body: bool,
    ) -> Outcome {
        let retain_body = retain_body || !operation.capture.is_empty();
        let mut ctx = self.ctx.clone().with_user(user.map(str::to_string));
        if retain_body {
            ctx.retain_body = true;
        }
        let client = self.client(user).clone();
        let vars = self.vars(user).clone();
        let headers = self.session_headers(user).to_vec();
        self.active_operation = Some(operation.id.clone());

        let mut outcome =
            crate::engine::execute_with_session(&ctx, operation, &client, &vars, &headers).await;

        if let Some(body) = &outcome.body {
            self.capture_into(user, &operation.capture, body);
        }

        outcome.redact(&ctx.redactor);
        outcome
    }

    /// Drop `user`'s session: the next [`ensure`](Self::ensure) re-authenticates
    /// with a fresh cookie jar (§7.7.3).
    pub(crate) fn reset(&mut self, user: Option<&str>) {
        self.sessions.remove(&session_key(user));
    }

    /// Turn a diagnostic into a failed [`Outcome`], for errors that happen
    /// before any operation ran (an unknown user, a failed login).
    pub(crate) fn fail(
        &self,
        user: Option<&str>,
        operation: &Operation,
        diagnostic: Diagnostic,
    ) -> Outcome {
        let ctx = self.ctx.clone().with_user(user.map(str::to_string));
        let diagnostics = Diagnostics::from(vec![diagnostic]);
        crate::engine::failure_outcome(&ctx, operation, &diagnostics)
    }

    /// The session's default headers for `user` (empty for no session).
    fn session_headers(&self, user: Option<&str>) -> &[(String, String)] {
        self.sessions
            .get(&session_key(user))
            .map_or(&[], |session| session.headers.as_slice())
    }

    /// Reject an undeclared user name, listing the declared ones.
    fn check_user(&self, user: Option<&str>) -> Option<Diagnostic> {
        let name = user?;
        if self.ctx.resolved.users.contains_key(name) {
            return None;
        }
        Some(Diagnostic {
            code: codes::E_UNKNOWN_USER,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: format!("unknown user `{name}`"),
            hint: Some(known_users(&self.ctx.resolved)),
        })
    }

    fn is_logged_in(&self, user: Option<&str>) -> bool {
        self.sessions
            .get(&session_key(user))
            .is_some_and(|session| session.logged_in)
    }

    /// Create the session for `user` if it does not exist yet.
    fn open(&mut self, user: Option<&str>) {
        let key = session_key(user);
        if !self.sessions.contains_key(&key) {
            let session = self.build_session(user);
            self.sessions.insert(key, session);
        }
    }

    fn mark_logged_in(&mut self, user: Option<&str>) {
        if let Some(session) = self.sessions.get_mut(&session_key(user)) {
            session.logged_in = true;
        }
    }

    /// Whether the user carries a static `token:` (and therefore never needs a
    /// login round-trip, §7.7).
    fn has_static_token(&self, user: Option<&str>) -> bool {
        let Some(spec) = user.and_then(|name| self.ctx.resolved.users.get(name)) else {
            return false;
        };
        spec.token
            .as_ref()
            .and_then(|token| token.as_ref())
            .is_some_and(|token| !token.is_empty())
    }

    /// Build a session: its own jar and client, the `token:` header, and the
    /// resolved `credentials.*` values the login operation templates from.
    fn build_session(&self, user: Option<&str>) -> Session {
        let jar = Arc::new(Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .timeout(self.ctx.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("testdet/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        let headers = static_token_header(&self.ctx.resolved, user)
            .filter(|(name, value)| {
                HeaderName::from_bytes(name.as_bytes()).is_ok()
                    && HeaderValue::from_str(value).is_ok()
            })
            .into_iter()
            .collect();

        let mut vars = BTreeMap::new();
        if let Some(spec) = user.and_then(|name| self.ctx.resolved.users.get(name)) {
            let empty = BTreeMap::new();
            for (key, raw) in &spec.credentials {
                let value = resolve_text(&self.ctx.resolved, &empty, raw);
                vars.insert(format!("credentials.{key}"), Value::String(value));
            }
        }

        Session {
            client,
            jar,
            vars,
            headers,
            logged_in: false,
        }
    }
}

/// A stable session key: the user name, or `None` for the anonymous session.
fn session_key(user: Option<&str>) -> Option<String> {
    user.map(str::to_string)
}

/// A human name for a user, for messages.
fn user_name(user: Option<&str>) -> &str {
    user.unwrap_or("anonymous")
}

/// The hint listing the declared users, sorted (`BTreeMap` order).
fn known_users(resolved: &Resolved) -> String {
    if resolved.users.is_empty() {
        return "declare users in testdet/users.yaml".to_string();
    }
    let names = resolved
        .users
        .keys()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("declared users: {names}")
}

/// The `token:` sugar header: `Authorization: Bearer <token>` by default,
/// or `<tokenHeader>: <token>` when renamed. The value is templated so
/// `token: "{{env.CI_TOKEN}}"` works.
fn static_token_header(resolved: &Resolved, user: Option<&str>) -> Option<(String, String)> {
    let spec = user.and_then(|name| resolved.users.get(name))?;
    let token = spec.token.as_ref()?.as_ref()?;
    if token.is_empty() {
        return None;
    }

    let empty = BTreeMap::new();
    let token = resolve_text(resolved, &empty, token);
    let header = spec
        .token_header
        .as_ref()
        .and_then(|header| header.as_ref())
        .cloned()
        .unwrap_or_else(|| "Authorization".to_string());
    let value = if header.eq_ignore_ascii_case("authorization") {
        format!("Bearer {token}")
    } else {
        token
    };
    Some((header, value))
}

/// Whether a login outcome actually authenticated the user: a 4xx/5xx
/// status, any GraphQL error, or a transport error means it did not (§7.7).
fn login_succeeded(outcome: &Outcome) -> bool {
    outcome.transport_error.is_none() && outcome.status < 400 && outcome.graphql_errors.is_empty()
}

/// A diagnostic for a login that did not authenticate: it names the user, the
/// login operation and the status, and never the response body (§7.12).
fn login_failure(user: Option<&str>, login_id: &str, outcome: &Outcome) -> Diagnostic {
    let detail = if let Some(error) = &outcome.transport_error {
        format!("transport error: {}", error.message)
    } else if !outcome.graphql_errors.is_empty() {
        let messages = outcome
            .graphql_errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        format!("graphql errors: {messages}")
    } else {
        format!("the login response had status {}", outcome.status)
    };
    Diagnostic {
        code: codes::E_LOGIN_FAILED,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!(
            "user `{}` could not log in with operation `{login_id}`: {detail}",
            user_name(user)
        ),
        hint: Some(
            "the login itself is never retried; check the credentials and the login operation"
                .to_string(),
        ),
    }
}

/// Resolve one template string for a session value, falling back to the raw
/// text when it cannot be resolved (a session must still be usable).
fn resolve_text(resolved: &Resolved, session: &BTreeMap<String, Value>, text: &str) -> String {
    let env = build_source(&resolved.secrets, Path::new(""));
    let files = BTreeMap::new();
    let resolver = Resolver {
        vars: &resolved.vars,
        session,
        env: env.as_ref(),
        files: &files,
    };
    match resolver.resolve_str(text) {
        Ok(Value::String(text)) => text,
        Ok(Value::Number(number)) => number.to_string(),
        Ok(Value::Bool(boolean)) => boolean.to_string(),
        _ => text.to_string(),
    }
}

/// A client used only if a lazily-built one is somehow missing; it keeps
/// [`SessionStore::client`] total without a panic.
fn fallback_client() -> &'static reqwest::Client {
    static FALLBACK: OnceLock<reqwest::Client> = OnceLock::new();
    FALLBACK.get_or_init(reqwest::Client::new)
}

/// Resolve a `capture:` path against a JSON body.
///
/// Dot-separated segments, each optionally followed by `[n]` array indices
/// (`data.users[0].id`, `data.users[0][1]`); a bare numeric segment is also an
/// array index (`data.users.0.id`). Any miss — a non-object where a key is
/// needed, an out-of-range index, a non-array where an index is needed — is
/// `None`, never a panic.
#[must_use]
pub fn resolve_capture_path(body: &Value, path: &str) -> Option<Value> {
    let mut current = body;

    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }

        let (name, mut rest) = match segment.find('[') {
            Some(index) => (&segment[..index], &segment[index..]),
            None => (segment, ""),
        };

        if !name.is_empty() {
            current = if name.bytes().all(|byte| byte.is_ascii_digit()) {
                current.as_array()?.get(name.parse::<usize>().ok()?)?
            } else {
                current.as_object()?.get(name)?
            };
        }

        while !rest.is_empty() {
            let inner = rest.strip_prefix('[')?;
            let close = inner.find(']')?;
            let index = inner[..close].parse::<usize>().ok()?;
            current = current.as_array()?.get(index)?;
            rest = &inner[close + 1..];
        }
    }

    Some(current.clone())
}
