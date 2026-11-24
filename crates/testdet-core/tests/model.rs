//! Tests for the config model types and their derived JSON Schemas.

use testdet_core::config::model::HeaderMap;
use testdet_core::config::schema;

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
