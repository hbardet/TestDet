//! Value-merge helpers for environment resolution (§7.19).
//!
//! One helper per data kind, mirroring the precedence table: scalars replace,
//! maps merge key by key (later wins), users merge by name over existing
//! entries only, and a header value of `None` means "unset this header"
//! rather than "leave it alone".

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::model::{HeaderMap, User};

/// Merge `overlay` over `base`, key by key; `None` means "unset".
pub(crate) fn merge_headers(base: &mut HeaderMap, overlay: &HeaderMap) {
    for (name, value) in &overlay.0 {
        match value {
            Some(value) => {
                base.0.insert(name.clone(), Some(value.clone()));
            }
            None => {
                base.0.remove(name);
            }
        }
    }
}

/// Merge `overlay` over `base`, key by key; later values replace wholesale.
pub(crate) fn merge_vars(base: &mut BTreeMap<String, Value>, overlay: &BTreeMap<String, Value>) {
    for (name, value) in overlay {
        base.insert(name.clone(), value.clone());
    }
}

/// Field-wise merge of an environment override over a declared user.
///
/// Scalar fields (`token`, `tokenHeader`, `login`) are doubly-optional:
/// absent keeps the inherited value, an explicit `null` (`Some(None)`) unsets
/// it, and a value (`Some(Some(v))`) replaces it; maps merge key by key
/// (`credentials`, `vars`); and `headers` merge with `None` meaning "unset".
pub(crate) fn merge_user(base: &User, overlay: &User) -> User {
    let mut merged = base.clone();

    for (name, value) in &overlay.credentials {
        merged.credentials.insert(name.clone(), value.clone());
    }
    if let Some(token) = &overlay.token {
        merged.token = Some(token.clone());
    }
    if let Some(header) = &overlay.token_header {
        merged.token_header = Some(header.clone());
    }
    if let Some(login) = &overlay.login {
        merged.login = Some(login.clone());
    }
    for (name, value) in &overlay.vars {
        merged.vars.insert(name.clone(), value.clone());
    }
    merge_headers(&mut merged.headers, &overlay.headers);

    merged
}
