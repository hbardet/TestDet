//! `testdet config` — print the fully resolved configuration (§7.19, §7.12).
//!
//! Every string value passes through the [`Redactor`], so a header echo or a
//! variable that happens to contain a secret is printed as `<NAME>-CENSORED`
//! rather than leaked.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use testdet_core::config::model::{HeaderMap, User};
use testdet_core::config::{Config, LoadOptions, Resolved, load_config, resolve};
use testdet_core::diagnostic::render_text as render_diagnostics;
use testdet_core::operation::model::{HttpRequest, Operation, Request};
use testdet_core::redact::{Redactor, redactor_for};
use testdet_core::secrets::{Origin, build_source};

use crate::exit::Exit;
use crate::format::Format;

/// Arguments for `testdet config`.
#[derive(Debug, Clone)]
pub struct ConfigArgs {
    /// Explicit `--config` path; `None` discovers by walking up from `cwd`.
    pub config: Option<PathBuf>,
    /// The environment to resolve (`--env`); `None` uses the manifest default.
    pub env: Option<String>,
    /// The user whose headers/vars layers are merged (`--user`).
    pub user: Option<String>,
    /// Output format (`--format`).
    pub format: Format,
}

/// Run `testdet config`.
///
/// Returns the exit code and the complete rendered output. A successful
/// [`Format::Json`] render is the only output that belongs on stdout; the
/// human [`Format::Text`] render, unknown-user usage errors and config-failure
/// diagnostics all go to stderr, which `testdet-cli::run` handles. A config
/// that fails to load prints its text diagnostics and exits 1; a usage error
/// (`--format ndjson`, an unknown `--user`) exits 3.
pub fn run(args: ConfigArgs) -> (Exit, String) {
    if args.format == Format::Ndjson {
        return (Exit::Usage, crate::commands::ndjson_usage_error());
    }

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };

    let config = match load_config(&LoadOptions {
        config: args.config,
        cwd,
        env: args.env.clone(),
    }) {
        Ok(config) => config,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_diagnostics(&diagnostics)),
    };

    let resolved = match resolve(&config, args.env.as_deref(), &[]) {
        Ok(resolved) => resolved,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_diagnostics(&diagnostics)),
    };

    if let Some(name) = args.user.as_deref()
        && !resolved.users.contains_key(name)
    {
        return (Exit::Usage, unknown_user(name, &resolved));
    }

    let redactor = redactor_for(&config, &resolved);
    let user = args.user.as_deref();

    let output = match args.format {
        Format::Json => render_json(&config, &resolved, user, &redactor),
        Format::Text => render_text(&config, &resolved, user, &redactor),
        Format::Ndjson => unreachable!("handled above"),
    };

    (Exit::Ok, output)
}

/// The usage error for a `--user` that names no user, with a hint listing the
/// known names (already sorted, since `Resolved.users` is a `BTreeMap`).
fn unknown_user(user: &str, resolved: &Resolved) -> String {
    let mut out = format!("unknown user `{user}`\n");
    if !resolved.users.is_empty() {
        let names = resolved
            .users
            .keys()
            .map(|known| format!("`{known}`"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("known users: {names}\n"));
    }
    out
}

/// The JSON form (ruling 5): `env`, `baseUrl`, `graphqlPath`, `headers`,
/// `vars`, `users` (`login`, `token` presence only) and `secrets` (`declared`,
/// `set`, `origin`). Pretty-printed; every string value is scrubbed.
fn render_json(
    config: &Config,
    resolved: &Resolved,
    user: Option<&str>,
    redactor: &Redactor,
) -> String {
    let empty = empty_operation();
    let headers = resolved.headers_for(&empty, user);
    let vars = resolved.vars_for(&empty, user);

    let mut root = serde_json::Map::new();
    root.insert("env".to_string(), Value::String(resolved.env_name.clone()));
    root.insert(
        "baseUrl".to_string(),
        Value::String(base_url_text(resolved.base_url.as_str())),
    );
    root.insert(
        "graphqlPath".to_string(),
        Value::String(resolved.graphql_path.clone()),
    );
    root.insert("headers".to_string(), headers_value(&headers));
    root.insert(
        "vars".to_string(),
        Value::Object(vars.into_iter().collect()),
    );
    root.insert("users".to_string(), users_value(&resolved.users));
    root.insert("secrets".to_string(), secrets_value(config, resolved));

    let mut root = Value::Object(root);
    redactor.scrub_json(&mut root);

    let mut out = serde_json::to_string_pretty(&root).expect("config view serializes");
    out.push('\n');
    out
}

/// The human form (ruling 5): `key: value` lines grouped under
/// `environment:`, `headers:`, `vars:`, `users:` and `secrets:` with two-space
/// indentation. Every value passes through the redactor.
fn render_text(
    config: &Config,
    resolved: &Resolved,
    user: Option<&str>,
    redactor: &Redactor,
) -> String {
    let empty = empty_operation();
    let headers = resolved.headers_for(&empty, user);
    let vars = resolved.vars_for(&empty, user);
    let source = build_source(&resolved.secrets, &config.root_dir);

    let mut out = String::new();

    out.push_str("environment:\n");
    out.push_str(&format!("  env: {}\n", redactor.scrub(&resolved.env_name)));
    out.push_str(&format!(
        "  baseUrl: {}\n",
        redactor.scrub(&base_url_text(resolved.base_url.as_str()))
    ));
    out.push_str(&format!(
        "  graphqlPath: {}\n",
        redactor.scrub(&resolved.graphql_path)
    ));

    out.push_str("headers:\n");
    for (name, value) in &headers.0 {
        let rendered = value
            .as_ref()
            .map(|v| redactor.scrub(v))
            .unwrap_or_else(|| "(unset)".to_string());
        out.push_str(&format!("  {name}: {rendered}\n"));
    }

    out.push_str("vars:\n");
    for (name, value) in &vars {
        out.push_str(&format!("  {name}: {}\n", var_text(value, redactor)));
    }

    out.push_str("users:\n");
    for (name, spec) in &resolved.users {
        out.push_str(&format!("  {name}:\n"));
        out.push_str(&format!(
            "    login: {}\n",
            login_text(&spec.login, redactor)
        ));
        out.push_str(&format!("    token: {}\n", token_presence(&spec.token)));
    }

    out.push_str("secrets:\n");
    if let Some(declaration) = &resolved.secrets.declaration {
        for name in declaration.0.keys() {
            let lookup = source.lookup(name);
            out.push_str(&format!("  {name}:\n"));
            out.push_str("    declared: true\n");
            out.push_str(&format!("    set: {}\n", lookup.value.is_some()));
            out.push_str(&format!("    origin: {}\n", origin_text(&lookup.origin)));
        }
    }

    out
}

/// The JSON object for a merged [`HeaderMap`]; `None` becomes `null`.
fn headers_value(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in &headers.0 {
        map.insert(
            name.clone(),
            value
                .as_ref()
                .map_or(Value::Null, |v| Value::String(v.clone())),
        );
    }
    Value::Object(map)
}

/// The JSON object for the users map: `login` and `token` presence only.
fn users_value(users: &BTreeMap<String, User>) -> Value {
    let mut map = serde_json::Map::new();
    for (name, spec) in users {
        let mut entry = serde_json::Map::new();
        entry.insert("login".to_string(), login_value(&spec.login));
        entry.insert(
            "token".to_string(),
            Value::String(token_presence(&spec.token).to_string()),
        );
        map.insert(name.clone(), Value::Object(entry));
    }
    Value::Object(map)
}

/// The JSON object for the secrets map: `declared`, `set` and `origin` per
/// declared name.
fn secrets_value(config: &Config, resolved: &Resolved) -> Value {
    let source = build_source(&resolved.secrets, &config.root_dir);
    let mut map = serde_json::Map::new();
    if let Some(declaration) = &resolved.secrets.declaration {
        for name in declaration.0.keys() {
            let lookup = source.lookup(name);
            let mut entry = serde_json::Map::new();
            entry.insert("declared".to_string(), Value::Bool(true));
            entry.insert("set".to_string(), Value::Bool(lookup.value.is_some()));
            entry.insert(
                "origin".to_string(),
                serde_json::to_value(&lookup.origin).unwrap_or_default(),
            );
            map.insert(name.clone(), Value::Object(entry));
        }
    }
    Value::Object(map)
}

/// The `login` value for the JSON form: the override, or `null`.
fn login_value(login: &Option<Option<String>>) -> Value {
    match login {
        Some(Some(value)) => Value::String(value.clone()),
        _ => Value::Null,
    }
}

/// The `login` value for the text form.
fn login_text(login: &Option<Option<String>>, redactor: &Redactor) -> String {
    match login {
        Some(Some(value)) => redactor.scrub(value),
        _ => "(none)".to_string(),
    }
}

/// A presence marker for a user token: never the value.
fn token_presence(token: &Option<Option<String>>) -> &'static str {
    match token {
        Some(Some(_)) => "set",
        Some(None) => "unset",
        None => "not-specified",
    }
}

/// The text form of a variable value (typed JSON), scrubbed.
fn var_text(value: &Value, redactor: &Redactor) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    redactor.scrub(&text)
}

/// The text form of a secret origin.
fn origin_text(origin: &Option<Origin>) -> String {
    match origin {
        Some(Origin::Shell) => "environment".to_string(),
        Some(Origin::Dotenv(path)) => path.display().to_string(),
        Some(Origin::Default) => "default".to_string(),
        None => "unset".to_string(),
    }
}

/// The `baseUrl` as the user wrote it: a bare origin (`scheme://host:port`)
/// prints without the trailing slash `reqwest::Url` normalizes in.
fn base_url_text(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let authority_start = scheme_end + 3;
        if let Some(path_start) = url[authority_start..].find('/')
            && url[authority_start + path_start..].len() == 1
        {
            return url[..authority_start + path_start].to_string();
        }
    }
    url.to_string()
}

/// An operation with no request headers, for `headers_for`/`vars_for` (ruling
/// 5: no operation is selected here, so the operation layer is empty).
fn empty_operation() -> Operation {
    Operation {
        id: String::new(),
        description: None,
        tags: Vec::new(),
        request: Request::Http(HttpRequest {
            method: String::new(),
            path: String::new(),
            body: None,
            headers: HeaderMap::default(),
        }),
        vars: BTreeMap::new(),
        files: BTreeMap::new(),
        capture: BTreeMap::new(),
    }
}
