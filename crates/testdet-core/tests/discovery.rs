//! Integration tests for config discovery and collection scanning.

use std::path::Path;

use tempfile::TempDir;

use testdet_core::config::discover::{
    Config, LoadOptions, discover, load_config, load_diagnostics,
};
use testdet_core::diagnostic::{Severity, codes};

/// Write `content` at `dir/<relative>`, creating parent directories.
fn write(dir: &Path, relative: &str, content: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A minimal valid manifest naming the given `collections` YAML list.
fn manifest_yaml(collections: &str) -> String {
    format!("version: 1\nname: my-api\ncollections: {collections}\n")
}

/// A minimal valid HTTP operation declaring `id`.
fn operation_yaml(id: &str) -> String {
    format!("id: {id}\nrequest:\n  kind: http\n  method: GET\n  path: /v1/{id}\n")
}

/// A `LoadOptions` rooted at `dir`, walking up from it with no `--config`.
fn from_cwd(dir: &Path) -> LoadOptions {
    LoadOptions {
        config: None,
        cwd: dir.to_path_buf(),
        env: None,
    }
}

#[test]
fn walks_up_from_a_nested_working_directory() {
    let root = TempDir::new().unwrap();
    write(root.path(), "testdet/testdet.yaml", &manifest_yaml("[]"));
    std::fs::create_dir_all(root.path().join("a/b")).unwrap();

    let found = discover(&from_cwd(&root.path().join("a/b"))).unwrap();
    let expected = std::fs::canonicalize(root.path().join("testdet/testdet.yaml")).unwrap();
    assert_eq!(std::fs::canonicalize(&found).unwrap(), expected);
}

#[test]
fn config_flag_accepts_a_directory_or_a_manifest_path() {
    let root = TempDir::new().unwrap();
    write(root.path(), "testdet/testdet.yaml", &manifest_yaml("[]"));
    let expected = std::fs::canonicalize(root.path().join("testdet/testdet.yaml")).unwrap();

    let manifest_file = root.path().join("testdet/testdet.yaml");

    for config in [
        Some(root.path().join("testdet")), // the config dir containing testdet.yaml
        Some(root.path().to_path_buf()),   // the parent dir containing testdet/
        Some(manifest_file),               // the manifest file itself
    ] {
        let opts = LoadOptions {
            config,
            cwd: root.path().to_path_buf(),
            env: None,
        };
        let found = discover(&opts).unwrap();
        assert_eq!(std::fs::canonicalize(&found).unwrap(), expected);
    }
}

#[test]
fn missing_manifest_is_one_clear_error() {
    let root = TempDir::new().unwrap();
    let diagnostics = discover(&from_cwd(root.path())).unwrap_err();

    assert_eq!(
        diagnostics.len(),
        1,
        "exactly one diagnostic, not one per parent"
    );
    let diagnostic = diagnostics.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_FILE);
    assert_eq!(diagnostic.severity, Severity::Error);
    assert!(
        diagnostic.message.contains("testdet/testdet.yaml"),
        "message names the manifest layout, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .message
            .contains(&root.path().display().to_string()),
        "message names the starting directory, got: {:?}",
        diagnostic.message
    );
    assert_eq!(
        diagnostic.hint.as_deref(),
        Some("run `testdet init`-style layout: create testdet/testdet.yaml")
    );
}

#[test]
fn scans_collections_recursively_and_rejects_duplicate_ids() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[operations]"),
    );
    write(
        root.path(),
        "testdet/operations/admin/getUser.yaml",
        &operation_yaml("getUser"),
    );
    write(
        root.path(),
        "testdet/operations/users/getUser.yaml",
        &operation_yaml("getUser"),
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();

    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_DUPLICATE_ID)
        .collect();
    assert_eq!(duplicates.len(), 1, "exactly one duplicate id is reported");

    let duplicate = duplicates[0];
    assert_eq!(duplicate.severity, Severity::Error);
    assert_eq!(duplicate.path.as_deref(), Some("id"));
    assert!(
        duplicate.span.is_some(),
        "duplicate id is attached to the `id` key"
    );

    // Scan order: admin < users, so the second file is users/getUser.yaml.
    let root_dir = std::fs::canonicalize(root.path().join("testdet")).unwrap();
    let second = root_dir.join("operations/users/getUser.yaml");
    assert_eq!(duplicate.file.as_deref(), Some(second.as_path()));

    assert!(
        duplicate.message.contains("operations/admin/getUser.yaml"),
        "message names the first file, got: {:?}",
        duplicate.message
    );
    assert!(
        duplicate.message.contains("operations/users/getUser.yaml"),
        "message names the second file, got: {:?}",
        duplicate.message
    );
}

#[test]
fn duplicate_ids_across_collections_are_rejected() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[one, two]"),
    );
    write(
        root.path(),
        "testdet/one/getUser.yaml",
        &operation_yaml("getUser"),
    );
    write(
        root.path(),
        "testdet/two/getUser.yaml",
        &operation_yaml("getUser"),
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();

    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_DUPLICATE_ID)
        .collect();
    assert_eq!(
        duplicates.len(),
        1,
        "the same id in two collections is one duplicate"
    );

    let duplicate = duplicates[0];
    assert_eq!(duplicate.path.as_deref(), Some("id"));

    // Scan order: one < two, so the second file is two/getUser.yaml.
    let root_dir = std::fs::canonicalize(root.path().join("testdet")).unwrap();
    let second = root_dir.join("two/getUser.yaml");
    assert_eq!(duplicate.file.as_deref(), Some(second.as_path()));

    assert!(
        duplicate.message.contains("one/getUser.yaml"),
        "message names the first file, got: {:?}",
        duplicate.message
    );
    assert!(
        duplicate.message.contains("two/getUser.yaml"),
        "message names the second file, got: {:?}",
        duplicate.message
    );
}

#[test]
fn id_not_matching_the_filename_stem_is_a_warning() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[operations]"),
    );
    write(
        root.path(),
        "testdet/operations/getUser.yaml",
        &operation_yaml("getUser2"),
    );

    let opts = from_cwd(root.path());

    // A warnings-only config loads successfully.
    let config = load_config(&opts).unwrap();
    assert!(config.operations.contains_key("getUser2"));
    assert_eq!(config.operations.len(), 1);

    // The warning is observable through the diagnostics-collecting call.
    let diagnostics = load_diagnostics(&opts);
    assert!(
        !diagnostics.has_errors(),
        "a filename mismatch is a warning, not an error"
    );
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::W_ID_FILENAME_MISMATCH)
        .collect();
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].severity, Severity::Warning);
    assert_eq!(mismatches[0].path.as_deref(), Some("id"));
    assert!(mismatches[0].span.is_some());
}

#[test]
fn missing_collection_directory_is_an_error() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[nope]"),
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();

    let missing: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNKNOWN_FILE)
        .collect();
    assert_eq!(missing.len(), 1);

    let resolved = std::fs::canonicalize(root.path().join("testdet"))
        .unwrap()
        .join("nope");
    assert!(
        missing[0].message.contains(&resolved.display().to_string()),
        "message names the resolved absolute path, got: {:?}",
        missing[0].message
    );
}

#[test]
fn relative_paths_resolve_against_the_config_directory_not_the_cwd() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[operations]"),
    );
    write(
        root.path(),
        "testdet/operations/getUser.yaml",
        &operation_yaml("getUser"),
    );

    // A decoy collection directory in the cwd must NOT be scanned.
    let elsewhere = TempDir::new().unwrap();
    write(
        elsewhere.path(),
        "operations/getUser.yaml",
        &operation_yaml("decoy"),
    );

    let opts = LoadOptions {
        config: Some(root.path().join("testdet/testdet.yaml")),
        cwd: elsewhere.path().to_path_buf(),
        env: None,
    };
    let config = load_config(&opts).unwrap();

    assert_eq!(config.operations.len(), 1);
    assert!(config.operations.contains_key("getUser"));
    assert!(!config.operations.contains_key("decoy"));
    assert_eq!(
        std::fs::canonicalize(&config.root_dir).unwrap(),
        std::fs::canonicalize(root.path().join("testdet")).unwrap()
    );
}

#[test]
fn users_file_is_optional() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest_yaml("[operations]"),
    );
    write(
        root.path(),
        "testdet/operations/getUser.yaml",
        &operation_yaml("getUser"),
    );

    let config: Config = load_config(&from_cwd(root.path())).unwrap();
    assert!(config.users.is_empty());
}
