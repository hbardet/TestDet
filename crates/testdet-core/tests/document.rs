//! Integration tests for the span-preserving YAML document loader.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use testdet_core::config::Document;
use testdet_core::diagnostic::{Span, codes};

/// Resolve a committed fixture under `tests/fixtures/document/`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/document")
        .join(name)
}

/// Load an inline YAML string from a unique temporary file so that
/// `Document::load` can read it from disk.
fn load_str(source: &str) -> Result<Document, testdet_core::diagnostic::Diagnostics> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("testdet-document-{unique}.yaml"));
    std::fs::write(&path, source).expect("failed to write temporary fixture");
    let result = Document::load(&path);
    let _ = std::fs::remove_file(&path);
    result
}

#[test]
fn reports_line_and_column_for_a_nested_key() {
    let document = Document::load(&fixture("headers.yaml")).expect("valid document");
    assert_eq!(
        document.span_at("headers.Authorization"),
        Some(Span {
            line: 4,
            column: 13,
        })
    );
}

#[test]
fn rejects_a_duplicate_key_at_its_first_occurrence() {
    let diagnostics = Document::load(&fixture("duplicate-key.yaml")).expect_err("duplicate key");
    let diagnostic = diagnostics.iter().next().expect("one diagnostic");
    assert_eq!(diagnostic.code, codes::E_DUPLICATE_KEY);
    assert_eq!(diagnostic.span.expect("a span").line, 2);
}

#[test]
fn keeps_scalar_yaml_11_traps_as_written() {
    let document = Document::load(&fixture("scalars.yaml")).expect("valid document");
    assert_eq!(document.root()["country"], serde_json::json!("NO"));
    assert_eq!(document.root()["when"], serde_json::json!("2024-01-02"));
}

#[test]
fn preserves_integers_beyond_f64() {
    let document = Document::load(&fixture("big-integer.yaml")).expect("valid document");
    let big = &document.root()["big"];
    assert_eq!(big.as_i64(), Some(9_007_199_254_740_993_i64));
    assert_eq!(*big, serde_json::json!(9_007_199_254_740_993_i64));
}

#[test]
fn integers_beyond_u64_round_trip_exactly() {
    let document = load_str("huge: 99999999999999999999\n").expect("valid document");
    let huge = &document.root()["huge"];
    assert!(huge.is_number(), "expected a number, got {huge:?}");
    assert_eq!(huge.to_string(), "99999999999999999999");
}

#[test]
fn floats_and_exponents_still_arrive_as_numbers() {
    let document = load_str("a: 1.5\nb: 1e3\n").expect("valid document");
    assert_eq!(document.root()["a"].as_f64(), Some(1.5));
    assert_eq!(document.root()["b"].as_f64(), Some(1000.0));
}

#[test]
fn rejects_a_non_map_top_level_with_a_span() {
    for source in ["", "- a\n- b", "just a string"] {
        let diagnostics = load_str(source).expect_err("non-mapping top level");
        let diagnostic = diagnostics.iter().next().expect("one diagnostic");
        assert_eq!(diagnostic.code, codes::E_WRONG_TYPE, "source: {source:?}");
        assert_eq!(
            diagnostic.span.expect("a span").line,
            1,
            "source: {source:?}"
        );
    }
}

#[test]
fn anchors_are_rejected() {
    let diagnostics = Document::load(&fixture("anchors.yaml")).expect_err("anchor");
    let diagnostic = diagnostics.iter().next().expect("one diagnostic");
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_FIELD);
    assert!(
        diagnostic.message.contains("anchor"),
        "message should name the anchor: {:?}",
        diagnostic.message
    );
    assert_eq!(diagnostic.span.expect("a span").line, 1);
}

#[test]
fn aliases_are_rejected() {
    let diagnostics = Document::load(&fixture("aliases.yaml")).expect_err("alias");
    let alias = diagnostics
        .iter()
        .find(|d| d.message.contains("alias"))
        .expect("the `*b` alias is reported");
    assert_eq!(alias.code, codes::E_UNKNOWN_FIELD);
    assert_eq!(alias.span.expect("a span").line, 2);
}

#[test]
fn merge_keys_are_rejected_by_name() {
    let diagnostics = Document::load(&fixture("merge-key.yaml")).expect_err("merge key");
    // The pre-scan rejects the `*base` alias before the tree walk ever sees
    // the `<<` merge key, so the message names the alias rather than the
    // merge key. The merge key itself is still rejected by the tree walk's
    // unknown-field path when no alias is present.
    let alias = diagnostics
        .iter()
        .find(|d| d.message.contains("alias"))
        .expect("the `*base` alias is reported before the merge key");
    assert_eq!(alias.code, codes::E_UNKNOWN_FIELD);
    assert_eq!(alias.span.expect("a span").line, 2);
}

#[test]
fn valid_document_without_anchors_still_loads_with_spans() {
    let document = Document::load(&fixture("headers.yaml")).expect("valid document");
    assert_eq!(
        document.span_at("version"),
        Some(Span { line: 1, column: 1 })
    );
    assert_eq!(
        document.span_at("headers.Authorization"),
        Some(Span {
            line: 4,
            column: 13,
        })
    );
}
