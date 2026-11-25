//! Environment resolution: selecting an environment, applying the §7.19
//! precedence table, and building the single [`Resolved`] value that every
//! later task reads.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::config::discover::Config;
use crate::config::merge::{merge_headers, merge_user, merge_vars};
use crate::config::model::{Environment, HeaderMap, SecretSource, SecretsDeclaration, User};
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::operation::model::Operation;

/// An environment's secret-source configuration after resolution.
///
/// `file` is already resolved against `root_dir`; `declaration` is the
/// environment's secrets declaration (`config.secrets`). Task 13 consumes this
/// in `build_source`; lookup is not implemented here.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    pub source: SecretSource,
    pub file: Option<PathBuf>,
    pub declaration: Option<SecretsDeclaration>,
}

/// The fully resolved configuration for one environment.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub env_name: String,
    pub base_url: reqwest::Url,
    pub graphql_path: String,
    pub headers: HeaderMap,
    pub vars: BTreeMap<String, Value>,
    pub users: BTreeMap<String, User>,
    /// Every operation, by id. Sessions resolve login operations through this
    /// map (§7.7.3); the engine does not need it otherwise.
    pub operations: BTreeMap<String, Operation>,
    /// `defaultLogin` from the users file: the login operation a user without
    /// a `login:` override falls back to (§7.7).
    pub default_login: Option<String>,
    pub secrets: SecretsConfig,
    pub timeout: Duration,
    pub retries: u32,

    /// The directory that holds `testdet/testdet.yaml`. Relative file paths
    /// in a `files:` block resolve against it (§7.14), so the file cache in a
    /// [`crate::RunContext`] needs it too.
    pub root_dir: PathBuf,

    /// Manifest defaults + environment headers (no user), the first two layers
    /// `headers_for` restarts from.
    base_headers: HeaderMap,
    /// The environment's variables, the layer `vars_for` restarts from.
    env_vars: BTreeMap<String, Value>,
    /// `--var` values parsed as strings, the top variable layer.
    cli_vars: BTreeMap<String, Value>,
}

/// Resolve `config` for one environment, applying every §7.19 layer.
///
/// `env` is the `--env` argument; `None` falls back to
/// `manifest.default_environment`, then to a synthetic `local` environment
/// when the manifest declares no environments. `cli_vars` are `--var`
/// `name=value` pairs.
pub fn resolve(
    config: &Config,
    env: Option<&str>,
    cli_vars: &[(String, String)],
) -> Result<Resolved, Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    let Some((env_name, environment)) = select_environment(config, env, &mut diagnostics) else {
        return Err(diagnostics);
    };

    let base_url = match resolve_base_url(config, &env_name, &environment, &mut diagnostics) {
        Some(url) => url,
        None => return Err(diagnostics),
    };

    let graphql_path = environment
        .graphql_path
        .clone()
        .unwrap_or_else(|| config.manifest.graphql_path.clone());

    let mut base_headers = config.manifest.defaults.headers.clone();
    merge_headers(&mut base_headers, &environment.headers);

    // Users: `config.users` (users.yaml) is the only declaration site; the
    // environment overrides fields of existing users and may never introduce one.
    let mut users = config.users.clone();
    for (name, override_user) in &environment.users {
        match users.get_mut(name) {
            Some(existing) => *existing = merge_user(existing, override_user),
            None => diagnostics.push(Diagnostic {
                code: codes::E_UNKNOWN_USER,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some(format!("environments.{env_name}.users.{name}")),
                message: format!("unknown user `{name}`"),
                hint: Some(format!(
                    "declare `{name}` in testdet/users.yaml; an environment may override a user, \
                     never introduce one"
                )),
            }),
        }
    }

    let env_vars = environment.vars.clone();
    let mut vars = env_vars.clone();

    let mut cli = BTreeMap::new();
    for (name, value) in cli_vars {
        cli.insert(name.clone(), Value::String(value.clone()));
    }
    merge_vars(&mut vars, &cli);

    let headers = base_headers.clone();

    let secrets = SecretsConfig {
        source: environment
            .secrets
            .as_ref()
            .map(|secrets| secrets.source)
            .unwrap_or_default(),
        file: environment
            .secrets
            .as_ref()
            .and_then(|secrets| secrets.file.as_ref())
            .map(|file| config.root_dir.join(file)),
        declaration: config.secrets.clone(),
    };

    let timeout = Duration::from_secs(
        environment
            .transport
            .as_ref()
            .and_then(|transport| transport.timeout)
            .unwrap_or(30),
    );
    let retries = environment
        .transport
        .as_ref()
        .and_then(|transport| transport.retries)
        .unwrap_or(0);

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

    Ok(Resolved {
        env_name,
        base_url,
        graphql_path,
        headers,
        vars,
        users,
        operations: config.operations.clone(),
        default_login: config.default_login.clone(),
        secrets,
        timeout,
        retries,
        root_dir: config.root_dir.clone(),
        base_headers,
        env_vars,
        cli_vars: cli,
    })
}

impl Resolved {
    /// Build the request URL for `path` against `base_url`.
    ///
    /// The base path is normalised to end with `/` and the incoming path's
    /// leading `/` is stripped before [`reqwest::Url::join`], so neither a
    /// doubled slash nor a lost base path prefix is possible. An absolute URL
    /// (`http://…`) is returned as-is.
    ///
    /// A failed join is never silently papered over: returning the base URL
    /// would send the request somewhere the config never asked for, so it
    /// comes back as an `E_UNRESOLVED_REF` diagnostic that `prepare` turns
    /// into a config error.
    ///
    /// # Errors
    ///
    /// `E_UNRESOLVED_REF` when the resolved path cannot be joined onto the
    /// base URL. The diagnostic is boxed so the success type stays small.
    pub fn url_for(&self, path: &str) -> Result<reqwest::Url, Box<Diagnostic>> {
        if let Ok(absolute) = reqwest::Url::parse(path)
            && !absolute.scheme().is_empty()
        {
            return Ok(absolute);
        }

        let mut base = self.base_url.clone();
        let base_path = base.path().to_string();
        if !base_path.ends_with('/') {
            base.set_path(&format!("{base_path}/"));
        }

        base.join(path.trim_start_matches('/')).map_err(|error| {
            Box::new(Diagnostic {
                code: codes::E_UNRESOLVED_REF,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some("request.path".to_string()),
                message: format!("cannot build a request URL from `{path}`: {error}"),
                hint: Some(format!(
                    "the path must be relative to the base URL `{}`",
                    self.base_url
                )),
            })
        })
    }

    /// The four header layers for `operation` made as `user`, lowest to
    /// highest: manifest defaults, environment, user, operation request.
    pub fn headers_for(&self, operation: &Operation, user: Option<&str>) -> HeaderMap {
        let mut headers = self.base_headers.clone();
        if let Some(name) = user
            && let Some(selected) = self.users.get(name)
        {
            merge_headers(&mut headers, &selected.headers);
        }
        merge_headers(&mut headers, operation.request.headers());
        headers
    }

    /// The four variable layers for `operation` made as `user`, lowest to
    /// highest: operation, environment, user, `--var`.
    pub fn vars_for(&self, operation: &Operation, user: Option<&str>) -> BTreeMap<String, Value> {
        let mut vars = operation.vars.clone();
        merge_vars(&mut vars, &self.env_vars);
        if let Some(name) = user
            && let Some(selected) = self.users.get(name)
        {
            merge_vars(&mut vars, &selected.vars);
        }
        merge_vars(&mut vars, &self.cli_vars);
        vars
    }
}

/// Select the target environment per the ruling-3 matrix.
///
/// `--env` wins, then `defaultEnvironment`, then a synthetic `local`
/// environment when the manifest declares none. Errors name the field (or
/// `--env`) and list the declared names.
fn select_environment(
    config: &Config,
    env: Option<&str>,
    diagnostics: &mut Diagnostics,
) -> Option<(String, Environment)> {
    if let Some(name) = env {
        if let Some(environment) = config.manifest.environments.get(name) {
            return Some((name.to_string(), environment.clone()));
        }
        // `--env local` is accepted on a shorthand config (no declared
        // environments), mirroring the synthetic fallback below.
        if name == "local" && config.manifest.environments.is_empty() {
            return Some((
                name.to_string(),
                Environment {
                    base_url: None,
                    ..Environment::default()
                },
            ));
        }
        let declared = declared_names(&config.manifest.environments);
        diagnostics.push(Diagnostic {
            code: codes::E_UNKNOWN_ENV,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: format!("unknown environment `{name}`"),
            hint: Some(declared_names_hint(&declared)),
        });
        return None;
    }

    if let Some(name) = &config.manifest.default_environment {
        if let Some(environment) = config.manifest.environments.get(name) {
            return Some((name.clone(), environment.clone()));
        }
        let declared = declared_names(&config.manifest.environments);
        diagnostics.push(Diagnostic {
            code: codes::E_UNKNOWN_ENV,
            severity: Severity::Error,
            file: None,
            span: None,
            path: Some("defaultEnvironment".to_string()),
            message: format!("unknown environment `{name}`"),
            hint: Some(declared_names_hint(&declared)),
        });
        return None;
    }

    if config.manifest.environments.is_empty() {
        return Some((
            "local".to_string(),
            Environment {
                base_url: None,
                ..Environment::default()
            },
        ));
    }

    let declared = declared_names(&config.manifest.environments);
    let list = declared
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    diagnostics.push(Diagnostic {
        code: codes::E_MISSING_FIELD,
        severity: Severity::Error,
        file: None,
        span: None,
        path: Some("defaultEnvironment".to_string()),
        message: "missing required field `defaultEnvironment`".to_string(),
        hint: Some(format!("name one of {list}, or pass `--env`")),
    });
    None
}

/// Resolve the selected environment's `baseUrl`, falling back to the manifest's
/// top-level `baseUrl`. The field path names `baseUrl` for the synthetic
/// `local` environment and `environments.<name>.baseUrl` otherwise.
fn resolve_base_url(
    config: &Config,
    env_name: &str,
    environment: &Environment,
    diagnostics: &mut Diagnostics,
) -> Option<reqwest::Url> {
    let synthetic = config.manifest.environments.is_empty();
    let raw = environment
        .base_url
        .clone()
        .or_else(|| config.manifest.base_url.clone());

    let Some(raw) = raw else {
        diagnostics.push(Diagnostic {
            code: codes::E_MISSING_FIELD,
            severity: Severity::Error,
            file: None,
            span: None,
            path: Some(base_url_path(synthetic, env_name)),
            message: "missing required field `baseUrl`".to_string(),
            hint: Some(
                "set `baseUrl`, or declare `environments:` and `defaultEnvironment`".to_string(),
            ),
        });
        return None;
    };

    match reqwest::Url::parse(&raw) {
        Ok(url) => Some(url),
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: None,
                span: None,
                path: Some(base_url_path(synthetic, env_name)),
                message: format!("`baseUrl` must be an absolute URL: {error}"),
                hint: None,
            });
            None
        }
    }
}

/// The diagnostic field path for a `baseUrl` problem.
fn base_url_path(synthetic: bool, env_name: &str) -> String {
    if synthetic {
        "baseUrl".to_string()
    } else {
        format!("environments.{env_name}.baseUrl")
    }
}

/// The declared environment names, already sorted (BTreeMap key order).
fn declared_names(environments: &BTreeMap<String, Environment>) -> Vec<String> {
    environments.keys().cloned().collect()
}

/// A hint listing the declared environment names, sorted.
fn declared_names_hint(names: &[String]) -> String {
    let list = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("declared environments: {list}")
}
