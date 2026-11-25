//! Serialisation contract tests for the boundary types (§7.9, §7.10).
//!
//! These pin the JSON shape of `Outcome` and its nested types: `body` is
//! base64 when retained and `null` when not (with `body_retained` spelling out
//! which), `Duration` values serialise as integer milliseconds, and redaction
//! removes every registered secret before anything reaches a frontend.

use std::time::Duration;

use bytes::Bytes;

use testdet_core::redact::Redactor;
use testdet_core::{
    CookieInfo, EnvelopeShape, GraphQLError, Outcome, RequestEcho, Timing, TransportError,
    TransportKind,
};

#[test]
fn outcome_serialises_body_as_null_when_retention_is_off() {
    let outcome = Outcome {
        operation: "getUser".into(),
        user: None,
        request: RequestEcho {
            method: "POST".into(),
            url: "http://localhost/graphql".into(),
            headers: vec![],
            body_size: 0,
        },
        status: 200,
        headers: vec![],
        cookies: vec![],
        body: None,
        body_retained: false,
        body_size: 1234,
        graphql_errors: vec![],
        timing: Timing {
            total: Duration::from_millis(1500),
            connect: None,
            ttfb: None,
        },
        transport_error: None,
        shape: Default::default(),
    };

    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["body"], serde_json::Value::Null);
    assert_eq!(json["body_retained"], false);
    assert_eq!(json["body_size"], 1234);
}

#[test]
fn a_retained_body_round_trips_as_base64() {
    let outcome = Outcome {
        operation: "getUser".into(),
        user: None,
        request: RequestEcho {
            method: "POST".into(),
            url: "http://localhost/graphql".into(),
            headers: vec![],
            body_size: 0,
        },
        status: 200,
        headers: vec![],
        cookies: vec![],
        body: Some(Bytes::from_static(b"hello world")),
        body_retained: true,
        body_size: 11,
        graphql_errors: vec![],
        timing: Timing {
            total: Duration::from_millis(1500),
            connect: None,
            ttfb: None,
        },
        transport_error: None,
        shape: Default::default(),
    };

    let json = serde_json::to_string(&outcome).unwrap();
    assert!(
        json.contains("\"aGVsbG8gd29ybGQ=\""),
        "body not base64: {json}"
    );

    let back: Outcome = serde_json::from_str(&json).unwrap();
    assert_eq!(back.body, Some(Bytes::from_static(b"hello world")));
    assert!(back.body_retained);
}

#[test]
fn outcome_request_echo_never_contains_an_unredacted_registered_value() {
    let redactor = Redactor::new();
    redactor.register("API_TOKEN", "sup3rs3cr3t-token");

    let mut outcome = Outcome {
        operation: "getUser".into(),
        user: None,
        request: RequestEcho {
            method: "POST".into(),
            url: "http://localhost/graphql?token=sup3rs3cr3t-token".into(),
            headers: vec![("Authorization".into(), "Bearer sup3rs3cr3t-token".into())],
            body_size: 0,
        },
        status: 200,
        headers: vec![("X-Echo".into(), "sup3rs3cr3t-token".into())],
        cookies: vec![CookieInfo {
            name: "session".into(),
            value: "sup3rs3cr3t-token".into(),
            domain: "localhost".into(),
            path: "/".into(),
            http_only: true,
            secure: true,
            expires: None,
        }],
        body: Some(Bytes::from(r#"{"token":"sup3rs3cr3t-token"}"#.to_string())),
        body_retained: true,
        body_size: 31,
        graphql_errors: vec![],
        timing: Timing {
            total: Duration::from_millis(1500),
            connect: None,
            ttfb: None,
        },
        transport_error: None,
        shape: Default::default(),
    };

    outcome.redact(&redactor);

    let json = serde_json::to_string(&outcome).unwrap();
    assert!(
        !json.contains("sup3rs3cr3t-token"),
        "raw token leaked: {json}"
    );
    assert!(
        json.contains("<API_TOKEN>-CENSORED"),
        "no censor tag: {json}"
    );
}

#[test]
fn transport_error_and_graphql_errors_can_coexist_with_a_status() {
    let outcome = Outcome {
        operation: "getUser".into(),
        user: None,
        request: RequestEcho {
            method: "POST".into(),
            url: "http://localhost/graphql".into(),
            headers: vec![],
            body_size: 0,
        },
        status: 502,
        headers: vec![],
        cookies: vec![],
        body: None,
        body_retained: false,
        body_size: 0,
        graphql_errors: vec![GraphQLError {
            message: "boom".into(),
            path: Some(vec!["data".into(), "user".into()]),
            extensions: Some(serde_json::json!({ "code": "INTERNAL" })),
        }],
        timing: Timing {
            total: Duration::from_millis(1500),
            connect: None,
            ttfb: None,
        },
        transport_error: Some(TransportError {
            kind: TransportKind::Read,
            message: "connection reset".into(),
        }),
        shape: Default::default(),
    };

    let json = serde_json::to_string(&outcome).unwrap();
    let back: Outcome = serde_json::from_str(&json).unwrap();

    assert_eq!(back.status, 502);
    assert_eq!(back.graphql_errors.len(), 1);
    assert_eq!(back.graphql_errors[0].message, "boom");
    assert_eq!(
        back.graphql_errors[0].path.as_deref(),
        Some(&["data".into(), "user".into()][..])
    );
    assert_eq!(
        back.transport_error.as_ref().unwrap().kind,
        TransportKind::Read
    );
    assert_eq!(
        back.transport_error.as_ref().unwrap().message,
        "connection reset"
    );
}

#[test]
fn durations_serialise_as_milliseconds() {
    let timing = Timing {
        total: Duration::from_millis(1500),
        connect: None,
        ttfb: Some(Duration::from_millis(12)),
    };

    let json = serde_json::to_value(&timing).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "total": 1500, "connect": null, "ttfb": 12 })
    );
}

/// A minimal 200 OK outcome carrying `body`, with `body_size` as the wire size.
fn outcome_with_body(body: &str, body_size: u64) -> Outcome {
    Outcome {
        operation: "getUser".into(),
        user: None,
        request: RequestEcho {
            method: "POST".into(),
            url: "http://localhost/graphql".into(),
            headers: vec![],
            body_size: 0,
        },
        status: 200,
        headers: vec![],
        cookies: vec![],
        body: Some(Bytes::from(body.to_string())),
        body_retained: true,
        body_size,
        graphql_errors: vec![],
        timing: Timing {
            total: Duration::from_millis(1),
            connect: None,
            ttfb: None,
        },
        transport_error: None,
        shape: EnvelopeShape::Ok,
    }
}

#[test]
fn f5_the_envelope_shape_serialises_and_defaults_to_ok() {
    let mut outcome = outcome_with_body("{}", 2);
    outcome.shape = EnvelopeShape::NotAnObject;

    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["envelope"], "not-an-object");

    // An older payload without `envelope` still parses, as a clean envelope.
    let mut old = json.clone();
    old.as_object_mut().unwrap().remove("envelope");
    let back: Outcome = serde_json::from_value(old).unwrap();
    assert_eq!(back.shape, EnvelopeShape::Ok);
}

#[test]
fn f6_redaction_keeps_the_original_body_size_and_the_json_shape() {
    const SECRET: &str = "sup3rs3cr3t-token-value-0123456789";

    // A pretty-printed body of exactly 200 bytes, containing the secret.
    let (text, before) = (0..400)
        .find_map(|pad| {
            let doc = serde_json::json!({ "token": SECRET, "note": "x".repeat(pad) });
            let text = serde_json::to_string_pretty(&doc).unwrap();
            (text.len() == 200).then_some((text, doc))
        })
        .expect("a 200-byte pretty body exists");

    let redactor = Redactor::new();
    redactor.register("API_TOKEN", SECRET);

    let size = text.len() as u64;
    let mut outcome = outcome_with_body(&text, size);
    outcome.redact(&redactor);

    // The wire size is never the rewritten size (F6)...
    assert_eq!(outcome.body_size, 200);
    assert_ne!(text.len(), 0);
    let body = outcome.body.expect("the body stays retained");
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    // ...and redaction scrubbed the value without dropping other fields.
    assert!(!rendered.contains(SECRET), "{rendered}");
    assert!(rendered.contains("<API_TOKEN>-CENSORED"), "{rendered}");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["note"], before["note"]);
}
