//! Typed config models: manifest, environments, users and secrets.
//!
//! Every struct rejects unknown fields (`#[serde(deny_unknown_fields)]`) and
//! maps YAML `camelCase` keys onto Rust `snake_case` fields. The same serde
//! attributes drive the JSON Schema derivation in [`crate::config::schema`],
//! so the key names, optionality and unknown-field rejection are defined
//! exactly once. Field names follow the design spec (§7.5, §7.7, §7.11).

use std::collections::BTreeMap;
use std::ops::Deref;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A config-format version marker.
///
/// Only the integer `1` is accepted; anything else (including other integers)
/// fails deserialisation rather than being clamped. The value is exposed as a
/// plain `u32` through [`Deref`], so `manifest.version` behaves like the
/// `u32` named in the task Interfaces contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Version(u32);

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value == 1 {
            Ok(Version(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "version must be 1, found {value}"
            )))
        }
    }
}

impl Deref for Version {
    type Target = u32;

    fn deref(&self) -> &u32 {
        &self.0
    }
}

/// A map of header name to value.
///
/// `Some(v)` sets the header to `v`; `None` (a `null` in YAML) means "unset
/// this header" during the merge described in §7.19; an absent key leaves the
/// header untouched. The distinction between absent and `None` is load-bearing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct HeaderMap(pub BTreeMap<String, Option<String>>);

/// The manifest file (`testdet.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    /// `version: 1`, mandatory and checked first (§7.5).
    pub version: Version,
    /// `name`
    pub name: Option<String>,
    /// `defaultEnvironment`
    pub default_environment: Option<String>,
    /// `collections`
    #[serde(default)]
    pub collections: Vec<String>,
    /// `graphqlPath`, the default path for `kind: graphql` requests.
    pub graphql_path: String,
    /// `baseUrl`, shorthand for a single generated `local` environment (§7.5).
    pub base_url: Option<String>,
    /// `defaults`
    #[serde(default)]
    pub defaults: Defaults,
    /// `environments` (optional per §7.5).
    #[serde(default)]
    pub environments: BTreeMap<String, Environment>,
}

/// Manifest-level defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Defaults {
    /// `headers`
    pub headers: HeaderMap,
}

/// A named environment. Every field is an override that merges over the
/// manifest-level value (Task 9); all are optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Environment {
    /// `baseUrl`
    pub base_url: Option<String>,
    /// `graphqlPath`
    pub graphql_path: Option<String>,
    /// `headers`
    pub headers: HeaderMap,
    /// `vars`
    pub vars: BTreeMap<String, String>,
    /// `users` (override by name; never introduces a new user).
    pub users: BTreeMap<String, User>,
    /// `secrets`
    pub secrets: Option<Secrets>,
    /// `transport`
    pub transport: Option<Transport>,
}

/// A user declaration or per-environment override.
///
/// All fields are optional so the same type serves both the full declaration
/// in `users.yaml` and a partial per-environment override (spec §7.5, §7.7).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct User {
    /// `credentials`, arbitrary string keys → string values.
    pub credentials: BTreeMap<String, String>,
    /// `token`, sugar for `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// `tokenHeader`, renames the header that `token` fills.
    pub token_header: Option<String>,
    /// `login`, an override of the users-file `defaultLogin`.
    pub login: Option<String>,
    /// `vars`, per-user templating variables.
    pub vars: BTreeMap<String, String>,
    /// `headers`
    pub headers: HeaderMap,
}

/// The users file (`testdet/users.yaml`), the only place users are declared.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct UsersFile {
    /// `defaultLogin`
    pub default_login: Option<String>,
    /// `users`
    pub users: BTreeMap<String, User>,
}

/// A secret value source (§7.11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretSource {
    /// `env`: read from the process environment.
    #[default]
    Env,
    /// `dotenv`: read from the pinned dotenv file named by [`Secrets::file`].
    Dotenv,
}

/// An environment's secret-source configuration (`{ source, file }`, §7.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Secrets {
    /// `source`
    pub source: SecretSource,
    /// `file`, only meaningful for `source: dotenv`.
    pub file: Option<String>,
}

/// The secrets declaration file (`testdet/secrets/<env>.yaml`, §7.11).
///
/// A map of secret name → spec. It states what the config expects, never a
/// value (except a harmless `default`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SecretsDeclaration(pub BTreeMap<String, SecretSpec>);

/// One secret declaration entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct SecretSpec {
    /// `description`
    pub description: Option<String>,
    /// `default`, committed only for harmless values (§7.11).
    #[serde(rename = "default")]
    pub default_value: Option<String>,
}

/// Transport settings (timeout / retries), §7.5.
///
/// The spec names TLS as a third transport setting but does not pin its
/// fields. TLS is intentionally not modelled yet, so this struct deliberately
/// does **not** carry `deny_unknown_fields`: it must not reject a `tls:` key
/// that a later task will model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Transport {
    /// `timeout`
    pub timeout: Option<u64>,
    /// `retries`
    pub retries: Option<u32>,
}
