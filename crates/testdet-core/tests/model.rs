//! Tests for the config model types and their derived JSON Schemas.

use testdet_core::config::model::{HeaderMap, Manifest};
use testdet_core::config::schema;

#[test]
fn collections_is_required_and_graphql_path_defaults() {
    // A manifest omitting `collections` is invalid (`E_MISSING_FIELD`).
    let err = serde_json::from_str::<Manifest>(r#"{"version": 1}"#).unwrap_err();
    assert!(err.to_string().contains("collections"));

    // With `collections` present the manifest parses, and `graphqlPath`
    // defaults to `/graphql`.
    let manifest: Manifest =
        serde_json::from_str(r#"{"version": 1, "collections": ["operations"]}"#).unwrap();
    assert_eq!(manifest.collections, vec!["operations".to_string()]);
    assert_eq!(manifest.graphql_path, "/graphql");
}

#[test]
fn version_other_than_one_is_rejected() {
    let err = serde_json::from_str::<Manifest>(r#"{"version": 2, "collections": ["operations"]}"#)
        .unwrap_err();
    assert!(err.to_string().contains("version must be 1"));
}

#[test]
fn schema_forbids_unknown_fields_everywhere() {
    let s = serde_json::to_value(schema::manifest()).unwrap();
    assert_eq!(s["additionalProperties"], false);
    assert!(s["properties"]["version"].is_object());
    assert!(
        s["properties"]["environments"]["additionalProperties"].is_object()
            || s["properties"]["environments"]["additionalProperties"] == false
    );
}

#[test]
fn header_map_distinguishes_absent_from_unset() {
    // JSON is a YAML subset, so a serde_json round-trip exercises the same
    // `HeaderMap` deserialisation the YAML loader will drive.
    let headers: HeaderMap = serde_json::from_str(r#"{"A": "x", "B": null}"#).unwrap();

    // A string value is `Some`.
    assert_eq!(headers.0.get("A"), Some(&Some("x".to_string())));
    // An explicit null is `None` ("unset this header").
    assert_eq!(headers.0.get("B"), Some(&None));
    // A missing key is absent from the map entirely.
    assert!(!headers.0.contains_key("C"));
}
