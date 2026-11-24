//! Config discovery and collection scan.
//!
//! [`discover`] locates the manifest file (`testdet/testdet.yaml`), either by
//! walking up from the working directory or by honouring an explicit
//! `--config` path. [`load_config`] then loads the manifest, walks the
//! collections it names for operation files, and assembles the typed
//! [`Config`]. Every relative path in the config resolves against `root_dir` —
//! the directory containing the manifest — never the process working
//! directory.
//!
//! Discovery and the scan run in a single pass over the filesystem. Every
//! file found is parsed; a bad file contributes its diagnostics without
//! stopping the scan. Output diagnostics are sorted by `(file, line, column)`
//! so downstream renderers and snapshots are deterministic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Document;
use crate::config::model::{Manifest, SecretsDeclaration, User};
use crate::config::parse::{deserialize_validated, parse_manifest, parse_operation, parse_users};
use crate::config::schema;
use crate::config::validate::SchemaKind;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::operation::model::Operation;

/// How to locate the config: an explicit `--config` path, plus the working
/// directory used for upward discovery when `config` is `None`.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub config: Option<PathBuf>,
    pub cwd: PathBuf,
}

/// The fully loaded config: manifest, the directory it lives in, and every
/// operation, user and secret declaration it names.
///
/// `root_dir` is the directory containing the manifest (for a standard layout
/// this is `…/testdet`). Every relative path in the config — collection
/// directories, `users.yaml`, `secrets/<env>.yaml` — resolves against it.
#[derive(Debug)]
pub struct Config {
    pub manifest: Manifest,
    pub root_dir: PathBuf,
    pub operations: BTreeMap<String, Operation>,
    pub users: BTreeMap<String, User>,
    pub secrets: SecretsDeclaration,
}

/// Locate the manifest file, returning its path.
///
/// With no `config`, walk `cwd` and its parents to the filesystem root; the
/// first directory containing `testdet/testdet.yaml` wins. With `config`, a
/// directory resolves to `<dir>/testdet.yaml` then `<dir>/testdet/testdet.yaml`,
/// and a file is used as-is.
pub fn discover(opts: &LoadOptions) -> Result<PathBuf, Diagnostics> {
    match &opts.config {
        Some(path) => resolve_explicit(path),
        None => walk_up(&opts.cwd),
    }
}

/// Load the full config. On error the returned [`Diagnostics`] carries every
/// problem found (warnings included); on success the config is returned and
/// warnings are discarded (see [`load_diagnostics`] to collect them).
pub fn load_config(opts: &LoadOptions) -> Result<Config, Diagnostics> {
    let (config, diagnostics) = build(opts);
    if diagnostics.has_errors() {
        return Err(diagnostics);
    }
    // `build` returns `Some(config)` exactly when the manifest was found and
    // parsed, which is the only way to reach here with no errors.
    Ok(config.expect("no errors implies the manifest was found and parsed"))
}

/// Re-run discovery and the collection scan to collect every diagnostic —
/// warnings included — without requiring a clean config. This is the hook
/// Task 12 uses to recompute warnings for a config that loaded successfully.
pub fn load_diagnostics(opts: &LoadOptions) -> Diagnostics {
    build(opts).1
}

/// The single pass: discover, load the manifest, scan collections, load the
/// users file and the secrets declaration, accumulating every diagnostic.
/// Returns the best-effort config (built once the manifest parses) and the
/// sorted diagnostics.
fn build(opts: &LoadOptions) -> (Option<Config>, Diagnostics) {
    let mut diagnostics = Diagnostics::new();

    let manifest_path = match discover(opts) {
        Ok(path) => path,
        Err(err) => {
            diagnostics.extend(err.iter().cloned());
            sort(&mut diagnostics);
            return (None, diagnostics);
        }
    };

    let manifest_doc = match Document::load(&manifest_path) {
        Ok(doc) => doc,
        Err(err) => {
            diagnostics.extend(err.iter().cloned());
            sort(&mut diagnostics);
            return (None, diagnostics);
        }
    };

    let manifest = match parse_manifest(&manifest_doc) {
        Ok(manifest) => manifest,
        Err(err) => {
            diagnostics.extend(err.iter().cloned());
            sort(&mut diagnostics);
            return (None, diagnostics);
        }
    };

    let root_dir = manifest_dir(&manifest_path);

    let mut operations = BTreeMap::new();
    let mut users = BTreeMap::new();
    let mut secrets = SecretsDeclaration::default();

    // Collections: order preserved, duplicates allowed and harmless.
    for collection in &manifest.collections {
        let dir = root_dir.join(collection);
        let resolved = match std::fs::canonicalize(&dir) {
            Ok(resolved) if resolved.is_dir() => resolved,
            _ => {
                diagnostics.push(Diagnostic {
                    code: codes::E_UNKNOWN_FILE,
                    severity: Severity::Error,
                    file: None,
                    span: None,
                    path: None,
                    message: format!("collection directory does not exist: `{}`", dir.display()),
                    hint: None,
                });
                continue;
            }
        };
        scan_collection(&resolved, &root_dir, &mut operations, &mut diagnostics);
    }

    // Users file (`<root_dir>/users.yaml`): optional; invalid → diagnostics.
    let users_path = root_dir.join("users.yaml");
    if users_path.is_file() {
        match Document::load(&users_path).and_then(|doc| parse_users(&doc)) {
            Ok(users_file) => users = users_file.users,
            Err(err) => diagnostics.extend(err.iter().cloned()),
        }
    }

    // Secrets declaration (`<root_dir>/secrets/<env>.yaml`): optional; invalid
    // → diagnostics. Missing default environment or file → empty declaration.
    if let Some(env) = &manifest.default_environment {
        let secrets_path = root_dir.join("secrets").join(format!("{env}.yaml"));
        if secrets_path.is_file() {
            match Document::load(&secrets_path).and_then(|doc| {
                deserialize_validated::<SecretsDeclaration>(
                    &doc,
                    &schema::secrets(),
                    SchemaKind::Secrets,
                )
            }) {
                Ok(declaration) => secrets = declaration,
                Err(err) => diagnostics.extend(err.iter().cloned()),
            }
        }
    }

    sort(&mut diagnostics);

    let config = Config {
        manifest,
        root_dir,
        operations,
        users,
        secrets,
    };
    (Some(config), diagnostics)
}

/// Resolve an explicit `--config` path: a directory resolves to
/// `<dir>/testdet.yaml` then `<dir>/testdet/testdet.yaml`, and a file is used
/// as-is. A path that is neither an existing file nor a directory (or a
/// directory with no manifest) is one error naming the path.
fn resolve_explicit(path: &Path) -> Result<PathBuf, Diagnostics> {
    if path.is_dir() {
        let direct = path.join("testdet.yaml");
        if direct.is_file() {
            return Ok(direct);
        }
        let nested = path.join("testdet").join("testdet.yaml");
        if nested.is_file() {
            return Ok(nested);
        }
        return Err(Diagnostics::from(vec![Diagnostic {
            code: codes::E_UNKNOWN_FILE,
            severity: Severity::Error,
            file: None,
            span: None,
            path: None,
            message: format!(
                "no manifest found in config directory `{}`: expected `testdet.yaml` or `testdet/testdet.yaml`",
                path.display()
            ),
            hint: None,
        }]));
    }

    if path.is_file() {
        return Ok(path.to_path_buf());
    }

    Err(Diagnostics::from(vec![Diagnostic {
        code: codes::E_UNKNOWN_FILE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("config path does not exist: `{}`", path.display()),
        hint: None,
    }]))
}

/// Walk `cwd` and its parents to the filesystem root; the first directory
/// containing `testdet/testdet.yaml` wins. No match → one `E_UNKNOWN_FILE`.
fn walk_up(cwd: &Path) -> Result<PathBuf, Diagnostics> {
    let start = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut dir = Some(start.clone());

    while let Some(current) = dir {
        let candidate = current.join("testdet").join("testdet.yaml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = current.parent().map(Path::to_path_buf);
    }

    Err(Diagnostics::from(vec![Diagnostic {
        code: codes::E_UNKNOWN_FILE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!(
            "no manifest found: looked for `testdet/testdet.yaml` from `{}` up to the filesystem root",
            start.display()
        ),
        hint: Some("run `testdet init`-style layout: create testdet/testdet.yaml".to_string()),
    }]))
}

/// The directory containing the manifest, made absolute.
fn manifest_dir(manifest_path: &Path) -> PathBuf {
    let absolute =
        std::fs::canonicalize(manifest_path).unwrap_or_else(|_| manifest_path.to_path_buf());
    absolute
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Recursively scan one collection directory for operation files, in a
/// deterministic (sorted) order. A file that fails to parse contributes its
/// diagnostics but no operation; the scan never stops at the first failure.
fn scan_collection(
    dir: &Path,
    root_dir: &Path,
    operations: &mut BTreeMap<String, Operation>,
    diagnostics: &mut Diagnostics,
) {
    let mut files = Vec::new();
    collect_yaml_files(dir, &mut files);

    // First-seen file path per id, so a duplicate can name both files.
    let mut sources: BTreeMap<String, PathBuf> = BTreeMap::new();

    for file in files {
        let doc = match Document::load(&file) {
            Ok(doc) => doc,
            Err(err) => {
                diagnostics.extend(err.iter().cloned());
                continue;
            }
        };

        let operation = match parse_operation(&doc) {
            Ok(operation) => operation,
            Err(err) => {
                diagnostics.extend(err.iter().cloned());
                continue;
            }
        };

        if let Some(first) = sources.get(&operation.id) {
            diagnostics.push(Diagnostic {
                code: codes::E_DUPLICATE_ID,
                severity: Severity::Error,
                file: Some(file.clone()),
                span: doc.span_at("id"),
                path: Some("id".to_string()),
                message: format!(
                    "duplicate operation id `{}` defined in both `{}` and `{}`",
                    operation.id,
                    relative_to_root(first, root_dir),
                    relative_to_root(&file, root_dir),
                ),
                hint: None,
            });
            continue;
        }

        if let Some(stem) = file.file_stem().and_then(|s| s.to_str())
            && stem != operation.id
        {
            diagnostics.push(Diagnostic {
                code: codes::W_ID_FILENAME_MISMATCH,
                severity: Severity::Warning,
                file: Some(file.clone()),
                span: doc.span_at("id"),
                path: Some("id".to_string()),
                message: format!(
                    "operation id `{}` does not match the file name `{}`",
                    operation.id, stem
                ),
                hint: Some(format!(
                    "rename the file to `{}.yaml` or change the id to `{}`",
                    operation.id, stem
                )),
            });
        }

        sources.insert(operation.id.clone(), file.clone());
        operations.insert(operation.id.clone(), operation);
    }
}

/// Recursively collect operation files (`*.yaml`/`*.yml`, case-sensitive,
/// skipping `users.yaml`) under `dir`, in deterministic sorted order.
fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|a| a.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out);
        } else if path.is_file() && is_operation_file(&path) {
            out.push(path);
        }
    }
}

/// Whether `path` is an operation file: a `yaml`/`yml` file that is not the
/// users file.
fn is_operation_file(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) == Some("users.yaml") {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yaml") | Some("yml")
    )
}

/// `path` expressed relative to `root_dir`, falling back to the full path.
fn relative_to_root(path: &Path, root_dir: &Path) -> String {
    path.strip_prefix(root_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Sort diagnostics by `(file, line, column)` with absent locations first.
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
