//! `testdet schema` — dump a server's GraphQL introspection result (§7.17).
//!
//! The command asks the selected environment's `graphqlPath` for the schema and
//! prints the result as JSON: to stdout, or to `--out` for a dump that can be
//! committed and diffed. `--legacy` sends the older, smaller query for servers
//! that reject `specifiedByURL` or `isRepeatable` (the schema-description field
//! goes with them), which would otherwise fail the whole introspection.
//!
//! It reuses the engine rather than a second HTTP client: the query is a normal
//! in-memory `graphql` operation sent through a [`SessionStore`], so cookies,
//! the selected `--user`'s authentication, transport retries and redaction are
//! exactly those of `run`.
//!
//! Key order is deterministic because `serde_json`'s object map is a
//! `BTreeMap` (the workspace does not enable `preserve_order`), so a committed
//! dump only changes when the schema does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use testdet_core::RunContext;
use testdet_core::config::{LoadOptions, load_config, resolve};
use testdet_core::diagnostic::render_text;
use testdet_core::execute;
use testdet_core::operation::model::{GraphqlRequest, Operation, Request};
use testdet_core::outcome::TransportKind;
use testdet_core::redact::redactor_for;
use testdet_core::session::{SessionStore, execute_for_user};

use crate::commands::run::unknown_user;
use crate::exit::Exit;

/// The modern introspection query: the full standard shape, including the
/// fields older servers reject (`specifiedByURL`, `isRepeatable` and the
/// schema's own `description`).
const INTROSPECTION_QUERY: &str = r#"query IntrospectionQuery {
  __schema {
    description
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives {
      name
      description
      locations
      args { ...InputValue }
      isRepeatable
    }
  }
}

fragment FullType on __Type {
  kind
  name
  description
  specifiedByURL
  fields(includeDeprecated: true) {
    name
    description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated
    deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) {
    name
    description
    isDeprecated
    deprecationReason
  }
  possibleTypes { ...TypeRef }
}

fragment InputValue on __InputValue {
  name
  description
  type { ...TypeRef }
  defaultValue
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
            ofType {
              kind
              name
            }
          }
        }
      }
    }
  }
}
"#;

/// The legacy introspection query: identical but without the three newer
/// fields, so a server built against an older GraphQL implementation accepts
/// it (§7.17).
const LEGACY_INTROSPECTION_QUERY: &str = r#"query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives {
      name
      description
      locations
      args { ...InputValue }
    }
  }
}

fragment FullType on __Type {
  kind
  name
  description
  fields(includeDeprecated: true) {
    name
    description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated
    deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) {
    name
    description
    isDeprecated
    deprecationReason
  }
  possibleTypes { ...TypeRef }
}

fragment InputValue on __InputValue {
  name
  description
  type { ...TypeRef }
  defaultValue
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
            ofType {
              kind
              name
            }
          }
        }
      }
    }
  }
}
"#;

/// The introspection query to send: the modern one, or the reduced `--legacy`
/// shape for older servers (§7.17).
#[must_use]
pub fn introspection_query(legacy: bool) -> &'static str {
    if legacy {
        LEGACY_INTROSPECTION_QUERY
    } else {
        INTROSPECTION_QUERY
    }
}

/// Arguments for `testdet schema`.
#[derive(Debug, Clone)]
pub struct SchemaArgs {
    /// `--env`: the environment to resolve; `None` uses the manifest default.
    pub env: Option<String>,
    /// `--user`: authenticate as this user before introspecting.
    pub user: Option<String>,
    /// `--out`: write the dump here instead of stdout.
    pub out: Option<PathBuf>,
    /// `--legacy`: send the older query for servers that reject the newer
    /// fields.
    pub legacy: bool,
    /// `--force`: overwrite an existing `--out` file.
    ///
    /// Added by Task 22: §7.17 lets `--out` refuse to overwrite, and a dump is
    /// a committed artefact, so silently replacing one is the wrong default.
    pub force: bool,
}

/// Run `testdet schema`.
///
/// Returns the exit code and the complete output, as every other command does;
/// `--out` writes the file here and leaves the output empty.
pub fn run(args: SchemaArgs) -> (Exit, String) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };
    run_in(args, &cwd)
}

/// [`run`] against an explicit working directory.
///
/// `schema` has no `--config` in §7.15, so the config is discovered by walking
/// up from the working directory. This seam lets a test supply that directory
/// instead of changing the process-global one.
pub fn run_in(args: SchemaArgs, cwd: &Path) -> (Exit, String) {
    // A dump is a committed artefact: refuse to replace one before doing any
    // work, so the refusal costs no request and cannot leave a half-written
    // file behind.
    if let Some(out) = &args.out
        && out.exists()
        && !args.force
    {
        return (
            Exit::Usage,
            format!(
                "`--out {}` already exists\nhint: pass `--force` to overwrite it\n",
                out.display()
            ),
        );
    }

    let config = match load_config(&LoadOptions {
        config: None,
        cwd: cwd.to_path_buf(),
        env: args.env.clone(),
    }) {
        Ok(config) => config,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    if let Some(user) = &args.user
        && !config.users.contains_key(user)
    {
        return (Exit::Usage, render_text(&unknown_user(user, &config)));
    }

    // The schema query takes no `--var` in §7.15, so `resolve` sees the empty
    // override list. A declared-but-unset secret is not checked here: the
    // introspection request does not reference one, and the engine reports an
    // unresolvable template if the environment's own headers do.
    let resolved = match resolve(&config, args.env.as_deref(), &[]) {
        Ok(resolved) => resolved,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    let endpoint = resolved.url_for(&resolved.graphql_path).map_or_else(
        |_| format!("{}{}", resolved.base_url, resolved.graphql_path),
        |url| url.to_string(),
    );

    let operation = introspection_operation(args.legacy);
    let redactor = Arc::new(redactor_for(&config, &resolved));
    let ctx = RunContext::new(Arc::new(resolved), redactor.clone())
        .with_user(args.user.clone())
        // The whole point of the command is the body.
        .with_retain_body(true)
        .with_events(None);
    let mut store = SessionStore::new(ctx);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not start the async runtime: {error}\n"),
            );
        }
    };

    let outcome = runtime.block_on(async {
        match args.user {
            // `--user` authenticates through the same path `run` uses, so a
            // login, its captures and its cookies behave identically.
            Some(_) => execute_for_user(&mut store, args.user.as_deref(), &operation).await,
            None => {
                let _ = store.ensure(None).await;
                execute(&mut store, &operation).await
            }
        }
    });

    if let Some(error) = &outcome.transport_error {
        // A request that could never be built is a config error (as in `run`);
        // anything else failed on the wire.
        let exit = if error.kind == TransportKind::Prepare {
            Exit::ConfigInvalid
        } else {
            Exit::TransportFailure
        };
        return (exit, failure(&endpoint, &error.message));
    }

    if !outcome.graphql_errors.is_empty() {
        let reasons: Vec<&str> = outcome
            .graphql_errors
            .iter()
            .map(|error| error.message.as_str())
            .collect();
        return (
            Exit::TransportFailure,
            failure(
                &endpoint,
                &format!("GraphQL errors: {}", reasons.join("; ")),
            ),
        );
    }

    if !(200..300).contains(&outcome.status) {
        return (
            Exit::TransportFailure,
            failure(&endpoint, &format!("HTTP status {}", outcome.status)),
        );
    }

    let Some(body) = outcome.body.as_deref() else {
        return (
            Exit::Internal,
            failure(&endpoint, "the response body was not retained"),
        );
    };
    let body: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => {
            return (
                Exit::Internal,
                failure(&endpoint, &format!("the response is not JSON: {error}")),
            );
        }
    };

    // §7.17 says "the JSON result" without fixing the wrapper. A dump is the
    // schema itself, so the GraphQL envelope is unwrapped: the output is the
    // `data` object (`{"__schema": …}`). A body that carries no `data` object
    // is written as received rather than thrown away.
    let result = match &body {
        Value::Object(map) => match map.get("data") {
            Some(data) if !data.is_null() => data.clone(),
            _ => body.clone(),
        },
        _ => body.clone(),
    };
    let pretty = match serde_json::to_string_pretty(&result) {
        Ok(pretty) => pretty,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not render the schema: {error}\n"),
            );
        }
    };
    // Everything printed or written goes through the redactor: a schema can
    // legally echo a value a secret was registered for.
    let output = format!("{}\n", redactor.scrub(&pretty));

    if let Some(path) = &args.out {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return (
                Exit::Internal,
                format!(
                    "internal error: could not create `{}`: {error}\n",
                    parent.display()
                ),
            );
        }
        if let Err(error) = std::fs::write(path, output.as_bytes()) {
            return (
                Exit::Internal,
                format!(
                    "internal error: could not write `{}`: {error}\n",
                    path.display()
                ),
            );
        }
        return (Exit::Ok, String::new());
    }

    (Exit::Ok, output)
}

/// The in-memory operation the engine sends: a `graphql` request for the
/// introspection query, with no variables and the manifest's `graphqlPath`.
fn introspection_operation(legacy: bool) -> Operation {
    Operation {
        id: "introspect".to_string(),
        description: Some("Introspection query generated by `testdet schema`".to_string()),
        tags: Vec::new(),
        request: Request::Graphql(GraphqlRequest {
            path: None,
            query: introspection_query(legacy).to_string(),
            operation_name: None,
            variables: Default::default(),
            headers: Default::default(),
        }),
        vars: Default::default(),
        files: Default::default(),
        capture: Default::default(),
    }
}

/// A failure message that always names the endpoint we asked (§7.17): a
/// refused introspection must never look like an empty schema.
fn failure(endpoint: &str, detail: &str) -> String {
    format!("error: introspection against {endpoint} failed: {detail}\n")
}
