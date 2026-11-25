//! `testdet config` — print the fully resolved configuration (§7.19, §7.12).
//!
//! The view is *resolved*, not a dump of templates: headers and variables go
//! through the same [`Resolver`] the engine uses, so what is printed is what
//! would be sent. Every string value then passes through the [`Redactor`], so a
//! resolved secret is printed as `<NAME>-CENSORED` rather than leaked. A
//! template that cannot resolve here (a `{{session.*}}` reference, an unset
//! secret) is kept literally and listed under `unresolved` — never silently
//! dropped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use testdet_core::config::model::{HeaderMap, User};
use testdet_core::config::{Config, LoadOptions, Resolved, load_config, resolve};
use testdet_core::diagnostic::render_text as render_diagnostics;
use testdet_core::operation::model::{HttpRequest, Operation, Request};
use testdet_core::redact::{Redactor, redactor_for};
use testdet_core::secrets::{Origin, build_source};
use testdet_core::template::Resolver;

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
/// Returns the exit code and the complete rendered output. Both the [`Format::Json`]
/// and the human [`Format::Text`] renders are DATA and go to stdout; only the
/// unknown-user usage error and config-failure diagnostics go to stderr, which
/// `testdet-cli::run` handles. A config that fails to load prints its text
/// diagnostics and exits 1; a usage error (`--format ndjson`, an unknown
/// `--user`) exits 3.
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
    let view = View::build(&resolved, user);

    let output = match args.format {
        Format::Json => render_json(&config, &resolved, &view, &redactor),
        Format::Text => render_text(&config, &resolved, &view, &redactor),
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
fn render_json(config: &Config, resolved: &Resolved, view: &View, redactor: &Redactor) -> String {
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
    root.insert("headers".to_string(), headers_value(&view.headers));
    root.insert(
        "vars".to_string(),
        Value::Object(
            view.vars
                .iter()
                .map(|(name, value)| (name.clone(), view_value_json(value)))
                .collect(),
        ),
    );
    root.insert("users".to_string(), users_value(&resolved.users));
    root.insert("secrets".to_string(), secrets_value(config, resolved));
    root.insert("unresolved".to_string(), unresolved_value(&view.unresolved));

    let mut root = Value::Object(root);
    redactor.scrub_json(&mut root);

    let mut out = serde_json::to_string_pretty(&root).expect("config view serializes");
    out.push('\n');
    out
}

/// The human form (ruling 5): `key: value` lines grouped under
/// `environment:`, `headers:`, `vars:`, `users:` and `secrets:` with two-space
/// indentation. Every value passes through the redactor.
fn render_text(config: &Config, resolved: &Resolved, view: &View, redactor: &Redactor) -> String {
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
    for (name, value) in &view.headers {
        out.push_str(&format!("  {name}: {}\n", view_text(value, redactor)));
    }

    out.push_str("vars:\n");
    for (name, value) in &view.vars {
        out.push_str(&format!("  {name}: {}\n", view_text(value, redactor)));
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

    // Never silent: every template the view could not resolve is listed with
    // the text as written (which is what would be sent if it stayed unresolved).
    if !view.unresolved.is_empty() {
        out.push_str("unresolved:\n");
        for entry in &view.unresolved {
            out.push_str(&format!(
                "  {}: {}\n",
                entry.path,
                redactor.scrub(&entry.template)
            ));
        }
    }

    out
}

/// One template the view could not resolve, kept so it can be reported instead
/// of silently dropped (F7).
struct Unresolved {
    /// Where it sits, e.g. `headers.authorization` or `vars.base`.
    path: String,
    /// The template exactly as written.
    template: String,
    /// The resolver's own message.
    message: String,
}

/// One header or variable in the resolved view.
struct ViewValue {
    /// The template as written (`None` for an unset header or a non-string
    /// variable).
    template: Option<String>,
    /// The resolved value, or `None` when it could not resolve here.
    resolved: Option<Value>,
}

/// The merged header and variable layers, resolved exactly as the engine
/// resolves them, plus every template that could not resolve (F7).
struct View {
    headers: Vec<(String, ViewValue)>,
    vars: Vec<(String, ViewValue)>,
    unresolved: Vec<Unresolved>,
}

impl View {
    /// Resolve the merged layers with the engine's [`Resolver`].
    ///
    /// The session is empty because `config` runs before any login, so a
    /// `{{session.*}}` reference is expected to fail: it is shown literally and
    /// reported in `unresolved`. The same is true of an unset `{{env.*}}`.
    fn build(resolved: &Resolved, user: Option<&str>) -> Self {
        let empty = empty_operation();
        let vars = resolved.vars_for(&empty, user);
        let session = BTreeMap::new();
        let env = build_source(&resolved.secrets, Path::new(""));
        let resolver = Resolver {
            vars: &vars,
            session: &session,
            env: env.as_ref(),
            files: &empty.files,
        };

        let mut view = Self {
            headers: Vec::new(),
            vars: Vec::new(),
            unresolved: Vec::new(),
        };

        let headers = resolved.headers_for(&empty, user);
        for (name, value) in &headers.0 {
            let template = value.clone();
            let resolved_value = template
                .as_ref()
                .and_then(|raw| view.resolve_entry(&resolver, format!("headers.{name}"), raw));
            view.headers.push((
                name.clone(),
                ViewValue {
                    template,
                    resolved: resolved_value,
                },
            ));
        }

        for (name, value) in &vars {
            match value {
                Value::String(raw) => {
                    let resolved_value = view.resolve_entry(&resolver, format!("vars.{name}"), raw);
                    view.vars.push((
                        name.clone(),
                        ViewValue {
                            template: Some(raw.clone()),
                            resolved: resolved_value,
                        },
                    ));
                }
                other => view.vars.push((
                    name.clone(),
                    ViewValue {
                        template: None,
                        resolved: Some(other.clone()),
                    },
                )),
            }
        }

        view
    }

    /// Resolve one string, recording a failure instead of propagating it.
    fn resolve_entry(&mut self, resolver: &Resolver<'_>, path: String, raw: &str) -> Option<Value> {
        match resolver.resolve_str(raw) {
            Ok(value) => Some(value),
            Err(diagnostics) => {
                let message = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                self.unresolved.push(Unresolved {
                    path,
                    template: raw.to_string(),
                    message,
                });
                None
            }
        }
    }
}

/// The text form of one view value.
///
/// A template stays visible — it is not a secret — with the censored resolved
/// value beside it, so `{{env.ADMIN_TOKEN}}` prints as
/// `{{env.ADMIN_TOKEN}} (<NAME>-CENSORED)` instead of leaking the value.
fn view_text(value: &ViewValue, redactor: &Redactor) -> String {
    let Some(template) = &value.template else {
        return match &value.resolved {
            Some(resolved) => var_text(resolved, redactor),
            None => "(unset)".to_string(),
        };
    };
    match &value.resolved {
        Some(resolved) => {
            let rendered = var_text(resolved, redactor);
            if template.contains("{{") && rendered != *template {
                format!("{template} ({rendered})")
            } else {
                rendered
            }
        }
        None => format!("{template} (unresolved)"),
    }
}

/// The JSON form of one view value: the resolved value, the literal template
/// when it could not resolve, or `null` for an unset header.
fn view_value_json(value: &ViewValue) -> Value {
    match (&value.resolved, &value.template) {
        (Some(resolved), _) => resolved.clone(),
        (None, Some(template)) => Value::String(template.clone()),
        (None, None) => Value::Null,
    }
}

/// The JSON array of unresolved templates.
fn unresolved_value(unresolved: &[Unresolved]) -> Value {
    Value::Array(
        unresolved
            .iter()
            .map(|entry| {
                let mut object = serde_json::Map::new();
                object.insert("path".to_string(), Value::String(entry.path.clone()));
                object.insert(
                    "template".to_string(),
                    Value::String(entry.template.clone()),
                );
                object.insert("message".to_string(), Value::String(entry.message.clone()));
                Value::Object(object)
            })
            .collect(),
    )
}

/// The JSON object for the resolved header layer; an unset header is `null`.
fn headers_value(headers: &[(String, ViewValue)]) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        map.insert(name.clone(), view_value_json(value));
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
