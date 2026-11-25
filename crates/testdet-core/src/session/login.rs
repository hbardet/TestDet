//! Login and auth-failure handling (§7.7).
//!
//! [`execute_for_user`] is the only place a login is retried: exactly once,
//! after an auth failure, and never after a login that itself failed.

use serde_json::Value;

use crate::config::Resolved;
use crate::operation::model::Operation;
use crate::outcome::Outcome;

use super::store::SessionStore;

/// Run `operation` as `user`, logging in first when needed.
///
/// The sequence is fixed (§7.7.3):
/// 1. [`SessionStore::ensure`] — validate the user, reuse a cached session, or
///    log in once;
/// 2. run the operation;
/// 3. if the outcome is an auth failure, drop the session, `ensure` **once**
///    more (one re-login) and run the operation once more. The second outcome
///    is returned whatever it is — there is no loop and no third attempt.
///
/// A failure in either `ensure` returns a failed outcome carrying that
/// diagnostic and never touches the operation itself, so a login that fails is
/// attempted exactly once.
pub async fn execute_for_user(
    store: &mut SessionStore,
    user: Option<&str>,
    operation: &Operation,
) -> Outcome {
    if let Err(diagnostic) = store.ensure(user).await {
        return store.fail(user, operation, diagnostic);
    }

    let retain_body = !operation.capture.is_empty();
    let first = store.run(user, operation, retain_body).await;
    if !is_auth_failure(&first) {
        store.capture_response(user, operation, &first);
        return first;
    }

    store.reset(user);
    if let Err(diagnostic) = store.ensure(user).await {
        return store.fail(user, operation, diagnostic);
    }

    let second = store.run(user, operation, retain_body).await;
    store.capture_response(user, operation, &second);
    second
}

/// Whether an outcome means "the caller is not authenticated".
///
/// - REST: an HTTP 401. A 403 is authorization, not authentication, and is a
///   legitimate result.
/// - GraphQL: any error whose `extensions.code` is `UNAUTHENTICATED` or
///   `FORBIDDEN`, regardless of the HTTP status (§7.7.3).
///
/// [`Outcome`] carries no protocol marker, so the 401 check applies to both
/// kinds; for GraphQL the extension code is checked independently of status.
#[must_use]
pub fn is_auth_failure(outcome: &Outcome) -> bool {
    if outcome.status == 401 {
        return true;
    }

    outcome.graphql_errors.iter().any(|error| {
        error
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.get("code"))
            .and_then(Value::as_str)
            .is_some_and(|code| code == "UNAUTHENTICATED" || code == "FORBIDDEN")
    })
}

/// The login operation for `user`: the user's `login:` override, else the
/// users-file `defaultLogin`. `Some(None)` (an explicit `login: null`) means
/// "no login", so the default does not apply.
pub(super) fn login_operation(resolved: &Resolved, user: Option<&str>) -> Option<String> {
    let spec = user.and_then(|name| resolved.users.get(name))?;
    match &spec.login {
        Some(login) => login.clone(),
        None => resolved.default_login.clone(),
    }
}
