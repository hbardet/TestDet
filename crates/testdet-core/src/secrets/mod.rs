//! Secret-source seam (§7.11).
//!
//! [`SecretSource`] is the trait every source satisfies; [`build_source`]
//! constructs the v1 concrete sources (shell, dotenv) from a resolved
//! [`SecretsConfig`]. [`required_vars`] and [`secret_warnings`] are the
//! read-only views over the config that the `env` command and `validate`'s
//! warnings consume — neither ever prints or stores a value (ruling 9).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::ser::SerializeStruct;

use crate::config::crosscheck::reference_surfaces;
use crate::config::discover::Config;
use crate::config::model::{SecretSource as SourceKind, SecretsDeclaration};
use crate::config::resolve::{SecretsConfig, resolve};
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::template::{Token, references};

pub mod dotenv;

/// The result of looking up one secret name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// The secret value, or `None` when the name is not set.
    pub value: Option<String>,
    /// Where the value came from, when present.
    pub origin: Option<Origin>,
}

/// Where a secret value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Read from the process environment.
    Shell,
    /// Read from a pinned dotenv file.
    Dotenv(PathBuf),
    /// A harmless committed `default`.
    Default,
}

/// The JSON shape of [`Origin`] (ruling 7): a tagged object
/// `{"kind":"shell"}`, `{"kind":"dotenv","path":…}` or `{"kind":"default"}`.
/// An unset lookup has `None` here, which `Option<Origin>` serialises as
/// `null`.
impl Serialize for Origin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Origin::Shell => {
                let mut s = serializer.serialize_struct("Origin", 1)?;
                s.serialize_field("kind", "shell")?;
                s.end()
            }
            Origin::Dotenv(path) => {
                let mut s = serializer.serialize_struct("Origin", 2)?;
                s.serialize_field("kind", "dotenv")?;
                s.serialize_field("path", path)?;
                s.end()
            }
            Origin::Default => {
                let mut s = serializer.serialize_struct("Origin", 1)?;
                s.serialize_field("kind", "default")?;
                s.end()
            }
        }
    }
}

/// A source of secret values.
///
/// The defaulted `names()` lets `{{env.X}}` suggest a close name
/// (`API_TOKN` → `API_TOKEN`) when a lookup misses. Sources that cannot
/// enumerate names return the default empty list.
pub trait SecretSource {
    fn lookup(&self, name: &str) -> Lookup;

    fn names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One required secret name and everything the `env` command reports about it.
///
/// It deliberately carries only booleans and an [`Origin`]: the value is never
/// stored, printed or serialised (ruling 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VarRequirement {
    /// Whether the name appears in `config.secrets` (the declaration file).
    pub declared: bool,
    /// Whether a value could be resolved.
    pub set: bool,
    /// Where the value came from, when set.
    pub origin: Option<Origin>,
    /// The operation ids that reference the name (sorted, deduped).
    pub used_by: Vec<String>,
}

/// Build the concrete source for a resolved [`SecretsConfig`].
///
/// `source: env` checks the shell, then the declaration default. `source:
/// dotenv` checks the shell, then the pinned file, then the declaration
/// default. `cfg.file` is normally absolute (Task 9 resolves it against
/// `root_dir`); a relative path is resolved against `root`. A missing or
/// unreadable dotenv file contributes nothing. The file is read exactly once,
/// here; `lookup` never touches the filesystem (ruling 2).
pub fn build_source(cfg: &SecretsConfig, root: &Path) -> Box<dyn SecretSource> {
    match cfg.source {
        SourceKind::Env => Box::new(ShellSource {
            declaration: cfg.declaration.clone(),
        }),
        SourceKind::Dotenv => {
            let path = cfg.file.as_ref().map(|file| {
                if file.is_absolute() {
                    file.clone()
                } else {
                    root.join(file)
                }
            });
            let values = path.as_deref().map(dotenv::parse_file).unwrap_or_default();
            Box::new(DotenvSource {
                path,
                values,
                declaration: cfg.declaration.clone(),
            })
        }
    }
}

/// Every `{{env.X}}` reference across the config's operation surface, with its
/// declared/set state and origin, keyed by name.
///
/// The scan surface is exactly the one [`crate::config::crosscheck::crosscheck`]
/// uses: query, variables, request path, body values, headers, vars and capture
/// values. The source is built for `manifest.default_environment` (or the
/// synthetic `local` environment when none is declared); when it cannot be
/// built every lookup is treated as unset.
#[must_use]
pub fn required_vars(config: &Config) -> BTreeMap<String, VarRequirement> {
    required_vars_for(config, None)
}

/// [`required_vars`], but for the environment named by `env` (`None` = the
/// manifest default). Task 13's `env` command uses this so `--env` changes the
/// source that fills `set`/`origin` (ruling 7).
#[must_use]
pub fn required_vars_for(config: &Config, env: Option<&str>) -> BTreeMap<String, VarRequirement> {
    let mut requirements = collect_requirements(config);
    if let Ok(resolved) = resolve(config, env, &[]) {
        let source = build_source(&resolved.secrets, &config.root_dir);
        for (name, requirement) in &mut requirements {
            let lookup = source.lookup(name);
            requirement.set = lookup.value.is_some();
            requirement.origin = lookup.origin;
        }
    }
    requirements
}

/// One `W_UNSET_SECRET` per declared-but-unset referenced name, for
/// `testdet validate` (ruling 6). Undeclared references are left to
/// [`crate::config::crosscheck::crosscheck`]'s `E_UNKNOWN_ENV` — never
/// duplicated here.
#[must_use]
pub fn secret_warnings(config: &Config) -> Diagnostics {
    let requirements = required_vars_for(config, None);
    let dotenv_file = dotenv_file(config);
    let env = config.manifest.default_environment.clone();
    let mut diagnostics = Diagnostics::new();

    for (name, requirement) in &requirements {
        if !requirement.declared || requirement.set {
            continue;
        }
        let mut hint = origin_chain_hint(dotenv_file.as_deref());
        if let Some(env) = &env {
            hint.push_str(&format!(" (declared in testdet/secrets/{env}.yaml)"));
        }
        diagnostics.push(Diagnostic {
            code: codes::W_UNSET_SECRET,
            severity: Severity::Warning,
            file: None,
            span: None,
            path: None,
            message: format!("secret `{name}` is declared but not set"),
            hint: Some(hint),
        });
    }

    diagnostics
}

// --- concrete sources -----------------------------------------------------

/// `source: env`: the shell, then the declaration default.
struct ShellSource {
    declaration: Option<SecretsDeclaration>,
}

impl SecretSource for ShellSource {
    fn lookup(&self, name: &str) -> Lookup {
        if let Ok(value) = std::env::var(name) {
            return Lookup {
                value: Some(value),
                origin: Some(Origin::Shell),
            };
        }
        default_lookup(&self.declaration, name)
    }

    fn names(&self) -> Vec<String> {
        self.declaration
            .as_ref()
            .map(|d| d.0.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// `source: dotenv`: the shell, then the pinned file, then the declaration
/// default. The file is parsed once at construction; `values` never changes.
struct DotenvSource {
    path: Option<PathBuf>,
    values: BTreeMap<String, String>,
    declaration: Option<SecretsDeclaration>,
}

impl SecretSource for DotenvSource {
    fn lookup(&self, name: &str) -> Lookup {
        if let Ok(value) = std::env::var(name) {
            return Lookup {
                value: Some(value),
                origin: Some(Origin::Shell),
            };
        }
        if let Some(value) = self.values.get(name) {
            return Lookup {
                value: Some(value.clone()),
                origin: Some(Origin::Dotenv(
                    self.path.clone().expect("values implies a path"),
                )),
            };
        }
        default_lookup(&self.declaration, name)
    }

    fn names(&self) -> Vec<String> {
        self.declaration
            .as_ref()
            .map(|d| d.0.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// The declaration `default:` fallback, or the fully-unset lookup.
fn default_lookup(declaration: &Option<SecretsDeclaration>, name: &str) -> Lookup {
    if let Some(spec) = declaration.as_ref().and_then(|d| d.0.get(name))
        && let Some(default) = &spec.default_value
    {
        return Lookup {
            value: Some(default.clone()),
            origin: Some(Origin::Default),
        };
    }
    Lookup {
        value: None,
        origin: None,
    }
}

// --- requirement collection -----------------------------------------------

/// Collect names, `declared` and `used_by` across every reference surface the
/// config has: the operations and `users.yaml` (credentials, `token`,
/// `tokenHeader`, `headers`, `vars`). `used_by` names the owning operation, or
/// [`USERS_SURFACE`] for a reference made from the users file, so a secret
/// used only there is still required, warned about and cross-checked.
/// `set` and `origin` are left `false`/`None` for the caller to fill.
fn collect_requirements(config: &Config) -> BTreeMap<String, VarRequirement> {
    let mut requirements: BTreeMap<String, VarRequirement> = BTreeMap::new();

    for surface in reference_surfaces(config) {
        for reference in references(&surface.value) {
            if let Token::Env(name) = &reference.token {
                let requirement =
                    requirements
                        .entry(name.clone())
                        .or_insert_with(|| VarRequirement {
                            declared: config
                                .secrets
                                .as_ref()
                                .is_some_and(|d| d.0.contains_key(name)),
                            set: false,
                            origin: None,
                            used_by: Vec::new(),
                        });
                requirement.used_by.push(surface.owner.clone());
            }
        }
    }

    for requirement in requirements.values_mut() {
        requirement.used_by.sort();
        requirement.used_by.dedup();
    }

    requirements
}

/// The dotenv file `env` would resolve for the manifest's default environment,
/// when that environment declares `source: dotenv`. Used only to name the
/// origin chain in a warning hint; never read here.
fn dotenv_file(config: &Config) -> Option<PathBuf> {
    let env_name = config.manifest.default_environment.as_deref()?;
    let environment = config.manifest.environments.get(env_name)?;
    let secrets = environment.secrets.as_ref()?;
    if secrets.source == SourceKind::Dotenv {
        secrets.file.as_ref().map(|file| config.root_dir.join(file))
    } else {
        None
    }
}

/// The origin-chain half of the `W_UNSET_SECRET` hint.
fn origin_chain_hint(dotenv_file: Option<&Path>) -> String {
    match dotenv_file {
        Some(file) => format!("set it in the environment or in `{}`", file.display()),
        None => "set it in the environment".to_string(),
    }
}
