//! Integration tests for the schema-driven validation pass.

use std::path::PathBuf;

use testdet_core::config::Document;
use testdet_core::config::schema;
use testdet_core::config::validate::{SchemaKind, validate_document};
use testdet_core::diagnostic::{Severity, codes};

/// Resolve a committed fixture under `tests/fixtures/invalid/`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/invalid")
        .join(name)
}

/// Load a fixture and validate it against the manifest schema.
fn validate_fixture(name: &str) -> testdet_core::diagnostic::Diagnostics {
    let document = Document::load(&fixture(name)).expect("fixture is loadable YAML");
    validate_document(&document, &schema::manifest(), SchemaKind::Manifest)
}

#[test]
fn reports_every_problem_not_just_the_first() {
    // The fixture declares `version: 2`, `unknownTop: 1`, and omits
    // `collections` (and `baseUrl`/`environments`, which are optional).
    let diags = validate_fixture("manifest-three-errors.yaml");
    let found: Vec<_> = diags.iter().map(|d| d.code).collect();
    assert!(found.contains(&codes::E_UNKNOWN_FIELD));
    assert!(found.contains(&codes::E_VERSION));
    assert!(found.contains(&codes::E_MISSING_FIELD));
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count(),
        3,
        "exactly three errors, got: {found:?}"
    );
}

#[test]
fn unknown_field_carries_a_span_a_path_and_a_suggestion() {
    // `defaults.defaultHeader` is unknown; the nearest legal sibling is
    // `defaults.headers`, and the hint must name it as a qualified path.
    let diags = validate_fixture("manifest-unknown-in-defaults.yaml");
    let unknown = diags
        .iter()
        .find(|d| d.code == codes::E_UNKNOWN_FIELD)
        .expect("the unknown field is reported");

    assert!(unknown.span.is_some(), "unknown field carries a span");
    assert_eq!(unknown.path.as_deref(), Some("defaults.defaultHeader"));
    let hint = unknown
        .hint
        .as_deref()
        .expect("unknown field carries a hint");
    assert!(
        hint.contains("defaults.headers"),
        "hint should name `defaults.headers`, got: {hint:?}"
    );
}

#[test]
fn missing_version_is_reported_once_not_twice() {
    // A manifest missing `version` but with `collections` present must yield
    // exactly one E_VERSION: the hand check owns the missing-version case, and
    // the schema's `required` must not also fire E_MISSING_FIELD for it.
    let source = "collections:\n  - operations\n";
    let diags = validate_manifest_str(source);
    assert_eq!(
        diags.iter().filter(|d| d.code == codes::E_VERSION).count(),
        1
    );
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code == codes::E_MISSING_FIELD)
            .count(),
        0,
        "the schema `required` must not double-report a missing `version`"
    );
    assert_eq!(diags.len(), 1);
}

#[test]
fn wrong_typed_field_maps_to_wrong_type() {
    // `collections` is a list of strings; a scalar is the wrong type.
    let source = "version: 1\ncollections: not-a-list\n";
    let diags = validate_manifest_str(source);
    assert!(
        diags.iter().any(|d| d.code == codes::E_WRONG_TYPE),
        "wrong-typed collections yields E_WRONG_TYPE"
    );
}

/// Validate an inline manifest YAML string from a unique temporary file.
fn validate_manifest_str(source: &str) -> testdet_core::diagnostic::Diagnostics {
    use std::time::{SystemTime, UNIX_EPOCH};
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("testdet-validate-{unique}.yaml"));
    std::fs::write(&path, source).expect("failed to write temporary fixture");
    let document = Document::load(&path).expect("valid document");
    let diags = validate_document(&document, &schema::manifest(), SchemaKind::Manifest);
    let _ = std::fs::remove_file(&path);
    diags
}
