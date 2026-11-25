//! `Redactor` tests: exact replacement, encoded forms, ordering, JSON walk,
//! live growth and the registration threshold (§7.12).

use testdet_core::redact::Redactor;

#[test]
fn scrubs_the_exact_value_and_replaces_it_with_the_name_tag() {
    let redactor = Redactor::new();
    redactor.register("API_TOKEN", "s3cr3t");

    assert_eq!(
        redactor.scrub("token=s3cr3t and again s3cr3t"),
        "token=<API_TOKEN>-CENSORED and again <API_TOKEN>-CENSORED"
    );

    // Idempotent: scrubbing the already-censored string changes nothing
    // because `<API_TOKEN>-CENSORED` contains no registered needle.
    let once = redactor.scrub("token=s3cr3t");
    assert_eq!(redactor.scrub(&once), once);
}

#[test]
fn scrubs_the_url_encoded_and_base64_forms_too() {
    let redactor = Redactor::new();
    redactor.register("API_TOKEN", "a b+c");

    // exact, percent-encoded (`a%20b%2Bc`), base64 (`YSBiK2M=`) and the
    // unpadded base64 variant (`YSBiK2M`).
    let output = redactor.scrub("a b+c | a%20b%2Bc | YSBiK2M= | YSBiK2M");
    assert_eq!(
        output,
        "<API_TOKEN>-CENSORED | <API_TOKEN>-CENSORED | <API_TOKEN>-CENSORED | <API_TOKEN>-CENSORED"
    );
}

#[test]
fn scrubbing_happens_before_truncation() {
    let redactor = Redactor::new();
    let token = "x".repeat(200);
    redactor.register("API_TOKEN", &token);

    let scrubbed = redactor.scrub(&token);
    let truncated = scrubbed.chars().take(20).collect::<String>();

    // The first 20 chars are the whole tag, never a prefix of the token.
    assert_eq!(truncated, "<API_TOKEN>-CENSORED");
    assert!(!truncated.contains('x'));
}

#[test]
fn scrubbing_walks_a_whole_json_document() {
    let redactor = Redactor::new();
    redactor.register("API_TOKEN", "s3cr3t");

    let mut doc = serde_json::json!({
        "query": "login with s3cr3t",
        "nested": {
            "token": "s3cr3t",
            "arr": ["s3cr3t", { "deep": "s3cr3t" }]
        },
        "key_s3cr3t_name": "untouched-key"
    });

    redactor.scrub_json(&mut doc);

    assert_eq!(doc["query"], "login with <API_TOKEN>-CENSORED");
    assert_eq!(doc["nested"]["token"], "<API_TOKEN>-CENSORED");
    assert_eq!(doc["nested"]["arr"][0], "<API_TOKEN>-CENSORED");
    assert_eq!(doc["nested"]["arr"][1]["deep"], "<API_TOKEN>-CENSORED");

    // Keys are never scrubbed, and an unrelated value is left alone.
    assert!(doc.get("key_s3cr3t_name").is_some());
    assert_eq!(doc["key_s3cr3t_name"], "untouched-key");
}

#[test]
fn the_registry_grows_after_construction() {
    let redactor = Redactor::new();
    assert_eq!(redactor.scrub("no secret here"), "no secret here");

    redactor.register("SESSION", "captured-token-value");
    assert_eq!(
        redactor.scrub("token=captured-token-value"),
        "token=<SESSION>-CENSORED"
    );
}

#[test]
fn f13_short_values_are_registered_exactly() {
    let redactor = Redactor::new();
    redactor.register("SHORT", "abc"); // 3 bytes: exact form only
    redactor.register("EMPTY", "");
    redactor.register("OK", "abcd"); // 4 bytes: encoded variants too

    // Any non-empty value is a secret, however short (F13).
    assert_eq!(redactor.scrub("abc"), "<SHORT>-CENSORED");
    // A short value does not get the percent/base64 variants, which would
    // censor unrelated output.
    assert_eq!(redactor.scrub("YWJj"), "YWJj");
    assert_eq!(redactor.scrub("abcd"), "<OK>-CENSORED");
    assert_eq!(
        redactor.names(),
        vec!["SHORT".to_string(), "OK".to_string()]
    );
    assert_eq!(redactor.scrub(""), "");
}

#[test]
fn f15_short_values_are_censored_only_as_whole_tokens() {
    let redactor = Redactor::new();
    redactor.register("CAPTURE_TT", "tt"); // 2 bytes
    redactor.register("CAPTURE_AB", "ab"); // 2 bytes

    // A 1–3 byte value is only a secret where it stands as a whole token:
    // `tt` inside `http` and `ab` inside `about` are unrelated output.
    assert_eq!(
        redactor.scrub("http://example.test/about"),
        "http://example.test/about"
    );

    // Where the value is bounded by non-token characters, it is censored.
    assert_eq!(redactor.scrub("x-t: ab"), "x-t: <CAPTURE_AB>-CENSORED");
    assert_eq!(redactor.scrub("?p=tt&"), "?p=<CAPTURE_TT>-CENSORED&");
}

#[test]
fn f15_a_three_byte_secret_inside_a_parameter_is_still_censored() {
    let redactor = Redactor::new();
    redactor.register("SHORT", "abc"); // 3 bytes: whole-token replacement

    assert_eq!(redactor.scrub("pw=abc;&"), "pw=<SHORT>-CENSORED;&");
    // Still whole-token only: `abc` inside a longer token is untouched.
    assert_eq!(redactor.scrub("x-abcd-y"), "x-abcd-y");
}

#[test]
fn f15_a_longer_secret_is_still_censored_inside_a_token() {
    let redactor = Redactor::new();
    redactor.register("LONG", "0123456789"); // 10 bytes: blind substring

    assert_eq!(redactor.scrub("xx0123456789yy"), "xx<LONG>-CENSOREDyy");
}
